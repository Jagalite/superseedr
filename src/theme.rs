// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use ratatui::style::{Color, Style};
use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize};
use strum::IntoEnumIterator;

use strum_macros::{Display, EnumIter};
use tracing::warn;

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, Display, Default)]
pub enum ThemeName {
    #[strum(serialize = "Andromeda")]
    Andromeda,
    #[strum(serialize = "Aurora")]
    Aurora,
    #[strum(serialize = "Ayu Dark")]
    AyuDark,
    #[strum(serialize = "Bubblegum")]
    Bubblegum,
    #[strum(serialize = "Catppuccin Latte")]
    CatppuccinLatte,
    #[strum(serialize = "Catppuccin Mocha")]
    #[default]
    CatppuccinMocha,
    #[strum(serialize = "Cyberpunk")]
    Cyberpunk,
    #[strum(serialize = "Deep Ocean")]
    DeepOcean,
    #[strum(serialize = "Deep Sky")]
    DeepSky,
    #[strum(serialize = "Diamond")]
    Diamond,
    #[strum(serialize = "Gold")]
    Gold,
    #[strum(serialize = "Dracula")]
    Dracula,
    #[strum(serialize = "Everforest Dark")]
    EverforestDark,
    #[strum(serialize = "GitHub Dark")]
    GitHubDark,
    #[strum(serialize = "GitHub Light")]
    GitHubLight,
    #[strum(serialize = "Gruvbox Dark")]
    GruvboxDark,
    #[strum(serialize = "Gruvbox Light")]
    GruvboxLight,
    #[strum(serialize = "Inferno")]
    Inferno,
    #[strum(serialize = "Kanagawa")]
    Kanagawa,
    #[strum(serialize = "Material Ocean")]
    MaterialOcean,
    #[strum(serialize = "Matrix")]
    Matrix,
    #[strum(serialize = "Monokai")]
    Monokai,
    #[strum(serialize = "Neon")]
    Neon,
    #[strum(serialize = "Nightfox")]
    Nightfox,
    #[strum(serialize = "Nord")]
    Nord,
    #[strum(serialize = "One Dark")]
    OneDark,
    #[strum(serialize = "Obsidian Forge")]
    ObsidianForge,
    #[strum(serialize = "Oxocarbon")]
    Oxocarbon,
    #[strum(serialize = "Arctic Whiteout")]
    ArcticWhiteout,
    #[strum(serialize = "PaperColor Light")]
    PaperColorLight,
    #[strum(serialize = "Bioluminescent Reef")]
    BioluminescentReef,
    #[strum(serialize = "Black Hole")]
    BlackHole,
    #[strum(serialize = "Rainbow")]
    Rainbow,
    #[strum(serialize = "Rose Pine")]
    RosePine,
    #[strum(serialize = "Solarized Dark")]
    SolarizedDark,
    #[strum(serialize = "Solarized Light")]
    SolarizedLight,
    #[strum(serialize = "Synthwave '84")]
    Synthwave84,
    #[strum(serialize = "Tokyo Night")]
    TokyoNight,
    #[strum(serialize = "Vesper")]
    Vesper,
    #[strum(serialize = "Zenburn")]
    Zenburn,
    #[strum(serialize = "Show")]
    Show,
    #[strum(serialize = "Sakura")]
    Sakura,
}

impl ThemeName {
    pub fn sorted_for_ui() -> Vec<Self> {
        let mut themes: Vec<Self> = Self::iter().collect();
        themes.sort_by_key(|theme| theme.to_string());
        themes
    }
}

impl Serialize for ThemeName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let s = match self {
            ThemeName::Andromeda => "andromeda",
            ThemeName::Aurora => "aurora",
            ThemeName::AyuDark => "ayu_dark",
            ThemeName::Bubblegum => "bubblegum",
            ThemeName::CatppuccinLatte => "catppuccin_latte",
            ThemeName::CatppuccinMocha => "catppuccin_mocha",
            ThemeName::Cyberpunk => "cyberpunk",
            ThemeName::DeepOcean => "deep_ocean",
            ThemeName::DeepSky => "deep_sky",
            ThemeName::Diamond => "diamond",
            ThemeName::Gold => "gold",
            ThemeName::Dracula => "dracula",
            ThemeName::EverforestDark => "everforest_dark",
            ThemeName::GitHubDark => "github_dark",
            ThemeName::GitHubLight => "github_light",
            ThemeName::GruvboxDark => "gruvbox_dark",
            ThemeName::GruvboxLight => "gruvbox_light",
            ThemeName::Inferno => "inferno",
            ThemeName::Kanagawa => "kanagawa",
            ThemeName::MaterialOcean => "material_ocean",
            ThemeName::Matrix => "matrix",
            ThemeName::Monokai => "monokai",
            ThemeName::Neon => "neon",
            ThemeName::Nightfox => "nightfox",
            ThemeName::Nord => "nord",
            ThemeName::OneDark => "one_dark",
            ThemeName::ObsidianForge => "obsidian_forge",
            ThemeName::Oxocarbon => "oxocarbon",
            ThemeName::ArcticWhiteout => "arctic_whiteout",
            ThemeName::PaperColorLight => "papercolor_light",
            ThemeName::BlackHole => "black_hole",
            ThemeName::BioluminescentReef => "bioluminescent_reef",
            ThemeName::Rainbow => "rainbow",
            ThemeName::RosePine => "rose_pine",
            ThemeName::SolarizedDark => "solarized_dark",
            ThemeName::SolarizedLight => "solarized_light",
            ThemeName::Synthwave84 => "synthwave_84",
            ThemeName::TokyoNight => "tokyo_night",
            ThemeName::Vesper => "vesper",
            ThemeName::Zenburn => "zenburn",
            ThemeName::Show => "show",
            ThemeName::Sakura => "sakura",
        };
        serializer.serialize_str(s)
    }
}

impl<'de> Deserialize<'de> for ThemeName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ThemeNameVisitor;

        impl<'de> Visitor<'de> for ThemeNameVisitor {
            type Value = ThemeName;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a theme name string")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(parse_theme_name(v))
            }

            fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(parse_theme_name(&v))
            }

            fn visit_bool<E>(self, _v: bool) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(ThemeName::default())
            }

            fn visit_i64<E>(self, _v: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(ThemeName::default())
            }

            fn visit_u64<E>(self, _v: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(ThemeName::default())
            }

            fn visit_f64<E>(self, _v: f64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(ThemeName::default())
            }

            fn visit_none<E>(self) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(ThemeName::default())
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(ThemeName::default())
            }

            fn visit_seq<A>(self, _seq: A) -> Result<Self::Value, A::Error>
            where
                A: de::SeqAccess<'de>,
            {
                Ok(ThemeName::default())
            }

            fn visit_map<A>(self, _map: A) -> Result<Self::Value, A::Error>
            where
                A: de::MapAccess<'de>,
            {
                Ok(ThemeName::default())
            }

            fn visit_some<D2>(self, deserializer: D2) -> Result<Self::Value, D2::Error>
            where
                D2: Deserializer<'de>,
            {
                deserializer.deserialize_any(ThemeNameVisitor)
            }
        }

        deserializer.deserialize_any(ThemeNameVisitor)
    }
}

enum ThemeResolution {
    Supported(ThemeName),
    Deprecated {
        replacement: ThemeName,
        deprecated_alias: &'static str,
    },
    Unknown,
}

fn parse_theme_name(raw: &str) -> ThemeName {
    match resolve_theme_name(raw) {
        ThemeResolution::Supported(theme) => theme,
        ThemeResolution::Deprecated {
            replacement,
            deprecated_alias,
        } => {
            warn!(
                "Theme '{}' is deprecated; using '{}'.",
                deprecated_alias, replacement
            );
            replacement
        }
        ThemeResolution::Unknown => {
            warn!(
                "Unknown theme '{}'; falling back to '{}'.",
                raw,
                ThemeName::default()
            );
            ThemeName::default()
        }
    }
}

fn resolve_theme_name(raw: &str) -> ThemeResolution {
    let normalized = normalize_theme_name_key(raw);
    if normalized.is_empty() {
        return ThemeResolution::Unknown;
    }

    let supported = match normalized.as_str() {
        "andromeda" => Some(ThemeName::Andromeda),
        "aurora" => Some(ThemeName::Aurora),
        "ayu_dark" => Some(ThemeName::AyuDark),
        "bubblegum" => Some(ThemeName::Bubblegum),
        "catppuccin_latte" => Some(ThemeName::CatppuccinLatte),
        "catppuccin_mocha" => Some(ThemeName::CatppuccinMocha),
        "cyberpunk" => Some(ThemeName::Cyberpunk),
        "deep_ocean" => Some(ThemeName::DeepOcean),
        "deep_sky" => Some(ThemeName::DeepSky),
        "diamond" => Some(ThemeName::Diamond),
        "gold" => Some(ThemeName::Gold),
        "dracula" => Some(ThemeName::Dracula),
        "everforest_dark" => Some(ThemeName::EverforestDark),
        "github_dark" => Some(ThemeName::GitHubDark),
        "github_light" => Some(ThemeName::GitHubLight),
        "gruvbox_dark" => Some(ThemeName::GruvboxDark),
        "gruvbox_light" => Some(ThemeName::GruvboxLight),
        "inferno" => Some(ThemeName::Inferno),
        "kanagawa" => Some(ThemeName::Kanagawa),
        "material_ocean" => Some(ThemeName::MaterialOcean),
        "matrix" => Some(ThemeName::Matrix),
        "monokai" => Some(ThemeName::Monokai),
        "neon" => Some(ThemeName::Neon),
        "nightfox" => Some(ThemeName::Nightfox),
        "nord" => Some(ThemeName::Nord),
        "one_dark" => Some(ThemeName::OneDark),
        "obsidian_forge" => Some(ThemeName::ObsidianForge),
        "oxocarbon" => Some(ThemeName::Oxocarbon),
        "arctic_whiteout" => Some(ThemeName::ArcticWhiteout),
        "papercolor_light" => Some(ThemeName::PaperColorLight),
        "black_hole" => Some(ThemeName::BlackHole),
        "bioluminescent_reef" => Some(ThemeName::BioluminescentReef),
        "rainbow" => Some(ThemeName::Rainbow),
        "rose_pine" => Some(ThemeName::RosePine),
        "solarized_dark" => Some(ThemeName::SolarizedDark),
        "solarized_light" => Some(ThemeName::SolarizedLight),
        "synthwave_84" => Some(ThemeName::Synthwave84),
        "tokyo_night" => Some(ThemeName::TokyoNight),
        "vesper" => Some(ThemeName::Vesper),
        "zenburn" => Some(ThemeName::Zenburn),
        "show" => Some(ThemeName::Show),
        "sakura" => Some(ThemeName::Sakura),
        _ => None,
    };

    if let Some(theme) = supported {
        return ThemeResolution::Supported(theme);
    }

    let deprecated = match normalized.as_str() {
        "catppuccin" => Some(("catppuccin", ThemeName::CatppuccinMocha)),
        "synthwave84" => Some(("synthwave84", ThemeName::Synthwave84)),
        "tokyonight" => Some(("tokyonight", ThemeName::TokyoNight)),
        "flowers" => Some(("flowers", ThemeName::Sakura)),
        _ => None,
    };

    if let Some((alias, replacement)) = deprecated {
        return ThemeResolution::Deprecated {
            replacement,
            deprecated_alias: alias,
        };
    }

    ThemeResolution::Unknown
}

fn normalize_theme_name_key(input: &str) -> String {
    input
        .trim()
        .to_lowercase()
        .replace('\'', "")
        .replace(['-', ' '], "_")
}

#[derive(Debug, Clone, Copy)]
pub struct ThemeEffects {
    pub local_enabled: bool,
    pub flicker_hz: f32,
    pub flicker_intensity: f32,
    pub local_burst_duty: f32,
    pub local_burst_hz: f32,
    pub local_idle_intensity: f32,
    pub local_burst_boost: f32,
    pub wave_enabled: bool,
    pub wave_hz: f32,
    pub wave_intensity: f32,
    pub wave_wavelength: f32,
    pub wave_angle_degrees: f32,
    pub wave_mode: WaveMode,
    pub particle: ThemeParticleEffect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaveMode {
    Linear,
    RadialOut,
    #[allow(dead_code)]
    RadialIn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParticleLayerMode {
    None,
    Background,
    #[allow(dead_code)]
    Foreground,
    #[allow(dead_code)]
    Both,
}

impl ParticleLayerMode {
    pub fn has_background(self) -> bool {
        matches!(self, Self::Background | Self::Both)
    }

    pub fn has_foreground(self) -> bool {
        matches!(self, Self::Foreground | Self::Both)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParticleProfile {
    None,
    Sakura,
    Matrix,
    Diamond,
    BioluminescentReef,
    BlackHole,
}

#[derive(Debug, Clone, Copy)]
pub struct ThemeParticleEffect {
    pub enabled: bool,
    pub layer_mode: ParticleLayerMode,
    pub profile: ParticleProfile,
    pub density: f32,
    pub speed: f32,
    pub intensity: f32,
}

impl Default for ThemeParticleEffect {
    fn default() -> Self {
        Self {
            enabled: false,
            layer_mode: ParticleLayerMode::None,
            profile: ParticleProfile::None,
            density: 0.0,
            speed: 0.0,
            intensity: 0.0,
        }
    }
}

impl Default for ThemeEffects {
    fn default() -> Self {
        Self {
            local_enabled: false,
            flicker_hz: 0.0,
            flicker_intensity: 0.0,
            // Preserve legacy behavior by default (always in burst, unchanged intensity).
            local_burst_duty: 1.0,
            local_burst_hz: 0.0,
            local_idle_intensity: 1.0,
            local_burst_boost: 1.0,
            wave_enabled: false,
            wave_hz: 0.0,
            wave_intensity: 0.0,
            wave_wavelength: 0.0,
            wave_angle_degrees: 0.0,
            wave_mode: WaveMode::Linear,
            particle: ThemeParticleEffect::default(),
        }
    }
}

impl ThemeEffects {
    pub fn enabled(&self) -> bool {
        self.local_enabled || self.wave_enabled || self.particle.enabled
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ThemeSemantic {
    pub text: Color,
    pub subtext0: Color,
    pub subtext1: Color,
    pub overlay0: Color,
    pub surface0: Color,
    pub surface1: Color,
    pub surface2: Color,
    pub border: Color,
    pub white: Color,
}

#[derive(Debug, Clone, Copy)]
pub struct ThemeHeatmap {
    pub low: Color,
    pub medium: Color,
    pub high: Color,
    pub empty: Color,
}

#[derive(Debug, Clone, Copy)]
pub struct ThemeStream {
    pub inflow: Color,
    pub outflow: Color,
}

#[derive(Debug, Clone, Copy)]
pub struct ThemeCategorical {
    pub rosewater: Color,
    pub flamingo: Color,
    pub pink: Color,
    pub mauve: Color,
    pub red: Color,
    pub maroon: Color,
    pub peach: Color,
    pub yellow: Color,
    pub green: Color,
    pub teal: Color,
    pub sky: Color,
    pub sapphire: Color,
    pub blue: Color,
    pub lavender: Color,
}

#[derive(Debug, Clone, Copy)]
pub struct ThemeScale {
    pub speed: [Color; 8],
    pub ip_hash: [Color; 14],
    pub heatmap: ThemeHeatmap,
    pub stream: ThemeStream,
    pub categorical: ThemeCategorical,
}

#[derive(Debug, Clone, Copy)]
pub struct ThemeStateSlots {
    pub error: Color,
    pub warning: Color,
    pub success: Color,
    pub info: Color,
    pub selected: Color,
    pub complete: Color,
}

#[derive(Debug, Clone, Copy)]
pub struct ThemeMetricSlots {
    pub download: Color,
    pub upload: Color,
}

#[derive(Debug, Clone, Copy)]
pub struct ThemePeerSlots {
    pub discovered: Color,
    pub connected: Color,
    pub disconnected: Color,
}

#[derive(Debug, Clone, Copy)]
pub struct ThemeAccentSlots {
    pub sky: Color,
    pub teal: Color,
    pub peach: Color,
    pub sapphire: Color,
    pub maroon: Color,
}

#[derive(Debug, Clone, Copy)]
pub struct ThemeRoleSlots {
    pub state: ThemeStateSlots,
    pub metric: ThemeMetricSlots,
    pub peer: ThemePeerSlots,
    pub accent: ThemeAccentSlots,
}

pub fn color_to_rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Reset => (255, 255, 255),
        Color::DarkGray => (128, 128, 128),
        Color::Red => (255, 0, 0),
        Color::LightRed => (255, 102, 102),
        Color::Green => (0, 255, 0),
        Color::LightGreen => (102, 255, 102),
        Color::Yellow => (255, 255, 0),
        Color::LightYellow => (255, 255, 153),
        Color::Blue => (0, 0, 255),
        Color::LightBlue => (102, 102, 255),
        Color::Magenta => (255, 0, 255),
        Color::LightMagenta => (255, 102, 255),
        Color::Cyan => (0, 255, 255),
        Color::LightCyan => (102, 255, 255),
        Color::Gray => (192, 192, 192),
        Color::White => (255, 255, 255),
        Color::Black => (0, 0, 0),
        Color::Indexed(i) => (i, i, i),
    }
}

pub fn blend_colors(c1: (u8, u8, u8), c2: (u8, u8, u8), ratio: f64) -> Color {
    let r = (c1.0 as f64 * (1.0 - ratio) + c2.0 as f64 * ratio) as u8;
    let g = (c1.1 as f64 * (1.0 - ratio) + c2.1 as f64 * ratio) as u8;
    let b = (c1.2 as f64 * (1.0 - ratio) + c2.2 as f64 * ratio) as u8;
    Color::Rgb(r, g, b)
}

#[derive(Debug, Clone, Copy)]
pub struct ThemeContext {
    pub theme: Theme,
    pub frame_time: f64,
}

impl ThemeContext {
    pub fn new(theme: Theme, frame_time: f64) -> Self {
        Self { theme, frame_time }
    }

    pub fn apply(&self, style: Style) -> Style {
        // Style construction stays deterministic; effects are applied once in the frame pass.
        style
    }

    pub fn state_error(&self) -> Color {
        self.theme.role_slots().state.error
    }

    pub fn state_warning(&self) -> Color {
        self.theme.role_slots().state.warning
    }

    pub fn state_success(&self) -> Color {
        self.theme.role_slots().state.success
    }

    pub fn state_info(&self) -> Color {
        self.theme.role_slots().state.info
    }

    pub fn state_selected(&self) -> Color {
        self.theme.role_slots().state.selected
    }

    pub fn state_complete(&self) -> Color {
        self.theme.role_slots().state.complete
    }

    pub fn metric_download(&self) -> Color {
        self.theme.role_slots().metric.download
    }

    pub fn metric_upload(&self) -> Color {
        self.theme.role_slots().metric.upload
    }

    pub fn peer_discovered(&self) -> Color {
        self.theme.role_slots().peer.discovered
    }

    pub fn peer_connected(&self) -> Color {
        self.theme.role_slots().peer.connected
    }

    pub fn peer_disconnected(&self) -> Color {
        self.theme.role_slots().peer.disconnected
    }

    pub fn accent_sky(&self) -> Color {
        self.theme.role_slots().accent.sky
    }

    pub fn accent_teal(&self) -> Color {
        self.theme.role_slots().accent.teal
    }

    pub fn accent_peach(&self) -> Color {
        self.theme.role_slots().accent.peach
    }

    pub fn accent_sapphire(&self) -> Color {
        self.theme.role_slots().accent.sapphire
    }

    pub fn accent_maroon(&self) -> Color {
        self.theme.role_slots().accent.maroon
    }
    pub fn apply_effects_to_color_at(
        &self,
        color: Color,
        x: u16,
        y: u16,
        frame_width: u16,
        frame_height: u16,
    ) -> Color {
        if !self.theme.effects.enabled() {
            return color;
        }

        let mut out = color;
        let (r, g, b) = color_to_rgb(color);

        if self.theme.effects.local_enabled {
            let freq = self.theme.effects.flicker_hz as f64;
            let intensity = self.theme.effects.flicker_intensity as f64;
            if intensity > 0.001 {
                let phase_offset = (r as f64 * 3.0 + g as f64 * 5.0 + b as f64 * 7.0) * 0.01;
                // Burst duty controls active flicker time; 1.0 preserves always-on behavior.
                let duty = self.theme.effects.local_burst_duty.clamp(0.0, 1.0) as f64;
                let burst_hz = if self.theme.effects.local_burst_hz <= 0.0 {
                    freq * 0.35
                } else {
                    self.theme.effects.local_burst_hz as f64
                };
                let idle_intensity = self.theme.effects.local_idle_intensity.clamp(0.0, 1.0) as f64;
                let burst_boost = self.theme.effects.local_burst_boost.max(0.0) as f64;
                let burst_gate =
                    (((self.frame_time * burst_hz) + (phase_offset * 0.75)).sin() + 1.0) / 2.0;
                let in_burst = burst_gate <= duty;
                let effective_intensity = if in_burst {
                    intensity * burst_boost
                } else {
                    intensity * idle_intensity
                };
                if effective_intensity <= 0.001 {
                    return out;
                }

                let base_wave = (self.frame_time * freq).sin();
                let drift_wave = ((self.frame_time * freq * 1.4) + phase_offset).sin();
                let wave = (base_wave + drift_wave) / 2.0;
                out = if wave > 0.0 {
                    let factor = wave * effective_intensity;
                    blend_colors((r, g, b), (255, 255, 255), factor)
                } else {
                    let factor = wave.abs() * (effective_intensity * 0.8);
                    blend_colors((r, g, b), (0, 0, 0), factor)
                };
            }
        }

        if self.theme.effects.wave_enabled {
            let wave_hz = self.theme.effects.wave_hz as f64;
            let intensity = self.theme.effects.wave_intensity as f64;
            let wavelength = self.theme.effects.wave_wavelength.max(1.0) as f64;
            let phase = match self.theme.effects.wave_mode {
                WaveMode::Linear => {
                    let angle = (self.theme.effects.wave_angle_degrees as f64).to_radians();
                    let dir_x = angle.cos();
                    let dir_y = angle.sin();
                    let pos = (x as f64 * dir_x + y as f64 * dir_y) / wavelength;
                    (self.frame_time * wave_hz * std::f64::consts::TAU) + pos
                }
                WaveMode::RadialOut => {
                    let cx = (frame_width.saturating_sub(1) as f64) * 0.5;
                    let cy = (frame_height.saturating_sub(1) as f64) * 0.5;
                    let dx = x as f64 - cx;
                    let dy = y as f64 - cy;
                    let dist = (dx * dx + dy * dy).sqrt() / wavelength;
                    (self.frame_time * wave_hz * std::f64::consts::TAU) - dist
                }
                WaveMode::RadialIn => {
                    let cx = (frame_width.saturating_sub(1) as f64) * 0.5;
                    let cy = (frame_height.saturating_sub(1) as f64) * 0.5;
                    let dx = x as f64 - cx;
                    let dy = y as f64 - cy;
                    let dist = (dx * dx + dy * dy).sqrt() / wavelength;
                    (self.frame_time * wave_hz * std::f64::consts::TAU) + dist
                }
            };
            let wave = phase.sin();

            let (rr, gg, bb) = color_to_rgb(out);
            out = if self.theme.name == ThemeName::BlackHole {
                // Black Hole uses a true dark sweep across all text instead of a brighten/darken cycle.
                let factor = ((wave + 1.0) * 0.5) * intensity;
                blend_colors((rr, gg, bb), (0, 0, 0), factor)
            } else if wave > 0.0 {
                let factor = wave * intensity;
                blend_colors((rr, gg, bb), (255, 255, 255), factor)
            } else {
                let factor = wave.abs() * intensity;
                blend_colors((rr, gg, bb), (0, 0, 0), factor)
            };
        }

        out
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub name: ThemeName,
    pub effects: ThemeEffects,
    pub semantic: ThemeSemantic,
    pub scale: ThemeScale,
}

impl Theme {
    pub fn role_slots(&self) -> ThemeRoleSlots {
        ThemeRoleSlots {
            state: ThemeStateSlots {
                error: self.scale.categorical.red,
                warning: self.scale.categorical.yellow,
                success: self.scale.categorical.green,
                info: self.scale.categorical.blue,
                selected: self.scale.categorical.mauve,
                complete: self.scale.categorical.lavender,
            },
            metric: ThemeMetricSlots {
                download: self.scale.categorical.sky,
                upload: self.scale.categorical.green,
            },
            peer: ThemePeerSlots {
                discovered: self.scale.categorical.yellow,
                connected: self.scale.categorical.teal,
                disconnected: self.scale.categorical.maroon,
            },
            accent: ThemeAccentSlots {
                sky: self.scale.categorical.sky,
                teal: self.scale.categorical.teal,
                peach: self.scale.categorical.peach,
                sapphire: self.scale.categorical.sapphire,
                maroon: self.scale.categorical.maroon,
            },
        }
    }

    pub fn builtin(name: ThemeName) -> Self {
        match name {
            ThemeName::Andromeda => Self::andromeda(),
            ThemeName::Aurora => Self::aurora(),
            ThemeName::AyuDark => Self::ayu_dark(),
            ThemeName::Bubblegum => Self::bubblegum(),
            ThemeName::CatppuccinLatte => Self::catppuccin_latte(),
            ThemeName::CatppuccinMocha => Self::catppuccin_mocha(),
            ThemeName::Cyberpunk => Self::cyberpunk(),
            ThemeName::DeepOcean => Self::deep_ocean(),
            ThemeName::DeepSky => Self::deep_sky(),
            ThemeName::Diamond => Self::diamond(),
            ThemeName::Gold => Self::gold(),
            ThemeName::Dracula => Self::dracula(),
            ThemeName::EverforestDark => Self::everforest_dark(),
            ThemeName::GitHubDark => Self::github_dark(),
            ThemeName::GitHubLight => Self::github_light(),
            ThemeName::GruvboxDark => Self::gruvbox_dark(),
            ThemeName::GruvboxLight => Self::gruvbox_light(),
            ThemeName::Inferno => Self::inferno(),
            ThemeName::Kanagawa => Self::kanagawa(),
            ThemeName::MaterialOcean => Self::material_ocean(),
            ThemeName::Matrix => Self::matrix(),
            ThemeName::Monokai => Self::monokai(),
            ThemeName::Neon => Self::neon(),
            ThemeName::Nightfox => Self::nightfox(),
            ThemeName::Nord => Self::nord(),
            ThemeName::OneDark => Self::one_dark(),
            ThemeName::ObsidianForge => Self::obsidian_forge(),
            ThemeName::Oxocarbon => Self::oxocarbon(),
            ThemeName::ArcticWhiteout => Self::arctic_whiteout(),
            ThemeName::PaperColorLight => Self::papercolor_light(),
            ThemeName::BlackHole => Self::black_hole(),
            ThemeName::BioluminescentReef => Self::bioluminescent_reef(),
            ThemeName::Rainbow => Self::rainbow(),
            ThemeName::RosePine => Self::rose_pine(),
            ThemeName::SolarizedDark => Self::solarized_dark(),
            ThemeName::SolarizedLight => Self::solarized_light(),
            ThemeName::Synthwave84 => Self::synthwave_84(),
            ThemeName::TokyoNight => Self::tokyo_night(),
            ThemeName::Vesper => Self::vesper(),
            ThemeName::Zenburn => Self::zenburn(),
            ThemeName::Show => Self::show(),
            ThemeName::Sakura => Self::sakura(),
        }
    }

    pub fn catppuccin_mocha() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(245, 224, 220),
            flamingo: Color::Rgb(242, 205, 205),
            pink: Color::Rgb(245, 194, 231),
            mauve: Color::Rgb(203, 166, 247),
            red: Color::Rgb(243, 139, 168),
            maroon: Color::Rgb(235, 160, 172),
            peach: Color::Rgb(250, 179, 135),
            yellow: Color::Rgb(249, 226, 175),
            green: Color::Rgb(166, 227, 161),
            teal: Color::Rgb(148, 226, 213),
            sky: Color::Rgb(137, 220, 235),
            sapphire: Color::Rgb(116, 199, 236),
            blue: Color::Rgb(137, 180, 250),
            lavender: Color::Rgb(180, 190, 254),
        };

        Self {
            name: ThemeName::CatppuccinMocha,
            effects: ThemeEffects::default(),
            semantic: ThemeSemantic {
                text: Color::Rgb(205, 214, 244),
                subtext1: Color::Rgb(186, 194, 222),
                subtext0: Color::Rgb(166, 173, 200),
                overlay0: Color::Rgb(108, 112, 134),
                surface2: Color::Rgb(88, 91, 112),
                surface1: Color::Rgb(69, 71, 90),
                surface0: Color::Rgb(49, 50, 68),
                border: Color::Rgb(108, 112, 134),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sky,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.maroon,
                    categorical.red,
                    categorical.flamingo,
                    categorical.pink,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.mauve,
                    medium: categorical.mauve,
                    high: categorical.mauve,
                    empty: Color::Rgb(69, 71, 90),
                },
                stream: ThemeStream {
                    inflow: categorical.blue,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn show() -> Self {
        // Keep distinct semantic status/metric colors. Show coordinates neutral
        // typography, borders and background geometry with hue-preserving accent light.
        let mut theme = Self::neon();
        theme.name = ThemeName::Show;
        theme.effects = ThemeEffects {
            local_enabled: true,
            ..ThemeEffects::default()
        };
        theme.semantic = ThemeSemantic {
            text: Color::Rgb(218, 232, 245),
            subtext1: Color::Rgb(170, 188, 210),
            subtext0: Color::Rgb(140, 156, 180),
            overlay0: Color::Rgb(58, 62, 86),
            surface2: Color::Rgb(30, 34, 54),
            surface1: Color::Rgb(40, 50, 76),
            surface0: Color::Rgb(5, 3, 9),
            border: Color::Rgb(104, 118, 158),
            white: Color::White,
        };
        theme
    }

    pub fn neon() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(255, 220, 245),
            flamingo: Color::Rgb(255, 150, 230),
            pink: Color::Rgb(255, 70, 230),
            mauve: Color::Rgb(210, 90, 255),
            red: Color::Rgb(255, 60, 120),
            maroon: Color::Rgb(255, 90, 160),
            peach: Color::Rgb(255, 170, 80),
            yellow: Color::Rgb(255, 240, 90),
            green: Color::Rgb(100, 255, 190),
            teal: Color::Rgb(0, 255, 255),
            sky: Color::Rgb(80, 220, 255),
            sapphire: Color::Rgb(40, 190, 255),
            blue: Color::Rgb(40, 110, 255),
            lavender: Color::Rgb(190, 170, 255),
        };

        Self {
            name: ThemeName::Neon,
            effects: ThemeEffects {
                local_enabled: true,
                flicker_hz: 9.0,
                flicker_intensity: 0.35,
                local_burst_duty: 0.08,
                local_burst_hz: 0.8,
                local_idle_intensity: 0.05,
                local_burst_boost: 1.20,
                ..ThemeEffects::default()
            },
            semantic: ThemeSemantic {
                text: Color::Rgb(230, 255, 255),
                subtext1: Color::Rgb(140, 230, 245),
                subtext0: Color::Rgb(90, 200, 220),
                overlay0: Color::Rgb(30, 70, 95),
                surface2: Color::Rgb(18, 40, 64),
                surface1: Color::Rgb(35, 70, 100),
                surface0: Color::Rgb(8, 22, 42),
                border: Color::Rgb(60, 100, 160),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    Color::Rgb(200, 255, 255),
                    Color::Rgb(120, 255, 240),
                    Color::Rgb(60, 245, 255),
                    Color::Rgb(80, 190, 255),
                    Color::Rgb(170, 120, 255),
                    Color::Rgb(255, 90, 230),
                    Color::Rgb(255, 60, 190),
                    Color::Rgb(255, 40, 150),
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.teal,
                    medium: categorical.teal,
                    high: categorical.teal,
                    empty: Color::Rgb(30, 45, 65),
                },
                stream: ThemeStream {
                    inflow: categorical.blue,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn bubblegum() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(255, 208, 235),
            flamingo: Color::Rgb(255, 173, 219),
            pink: Color::Rgb(245, 132, 204),
            mauve: Color::Rgb(213, 126, 203),
            red: Color::Rgb(236, 92, 149),
            maroon: Color::Rgb(191, 79, 128),
            peach: Color::Rgb(255, 172, 194),
            yellow: Color::Rgb(255, 198, 216),
            green: Color::Rgb(229, 160, 207),
            teal: Color::Rgb(217, 149, 212),
            sky: Color::Rgb(205, 141, 224),
            sapphire: Color::Rgb(193, 132, 217),
            blue: Color::Rgb(181, 122, 206),
            lavender: Color::Rgb(210, 172, 238),
        };

        Self {
            name: ThemeName::Bubblegum,
            effects: ThemeEffects {
                wave_enabled: true,
                wave_hz: 0.45,
                wave_intensity: 0.08,
                wave_wavelength: 26.0,
                wave_mode: WaveMode::RadialOut,
                ..ThemeEffects::default()
            },
            semantic: ThemeSemantic {
                text: Color::Rgb(255, 236, 247),
                subtext1: Color::Rgb(245, 206, 228),
                subtext0: Color::Rgb(224, 176, 207),
                overlay0: Color::Rgb(171, 117, 152),
                surface2: Color::Rgb(112, 67, 98),
                surface1: Color::Rgb(89, 50, 78),
                surface0: Color::Rgb(63, 31, 56),
                border: Color::Rgb(203, 142, 180),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    Color::Rgb(255, 240, 250),
                    Color::Rgb(255, 220, 240),
                    Color::Rgb(255, 200, 230),
                    Color::Rgb(255, 180, 220),
                    Color::Rgb(255, 160, 210),
                    Color::Rgb(255, 140, 205),
                    Color::Rgb(255, 120, 200),
                    Color::Rgb(255, 100, 195),
                ],
                ip_hash: [
                    categorical.rosewater,
                    categorical.flamingo,
                    categorical.pink,
                    categorical.mauve,
                    categorical.red,
                    categorical.maroon,
                    categorical.peach,
                    categorical.yellow,
                    categorical.lavender,
                    categorical.teal,
                    categorical.green,
                    categorical.sky,
                    categorical.sapphire,
                    categorical.blue,
                ],
                heatmap: ThemeHeatmap {
                    low: categorical.mauve,
                    medium: categorical.mauve,
                    high: categorical.mauve,
                    empty: Color::Rgb(89, 50, 78),
                },
                stream: ThemeStream {
                    inflow: categorical.mauve,
                    outflow: categorical.pink,
                },
                categorical,
            },
        }
    }

    pub fn deep_ocean() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(206, 233, 255),
            flamingo: Color::Rgb(168, 212, 247),
            pink: Color::Rgb(143, 191, 236),
            mauve: Color::Rgb(110, 165, 222),
            red: Color::Rgb(86, 142, 206),
            maroon: Color::Rgb(68, 123, 186),
            peach: Color::Rgb(52, 106, 168),
            yellow: Color::Rgb(37, 90, 151),
            green: Color::Rgb(31, 78, 136),
            teal: Color::Rgb(25, 67, 121),
            sky: Color::Rgb(19, 57, 106),
            sapphire: Color::Rgb(14, 47, 92),
            blue: Color::Rgb(10, 38, 79),
            lavender: Color::Rgb(7, 31, 67),
        };

        Self {
            name: ThemeName::DeepOcean,
            effects: ThemeEffects {
                wave_enabled: true,
                wave_hz: 0.32,
                wave_intensity: 0.18,
                wave_wavelength: 52.0,
                wave_angle_degrees: -72.0,
                wave_mode: WaveMode::Linear,
                ..ThemeEffects::default()
            },
            semantic: ThemeSemantic {
                text: Color::Rgb(178, 217, 247),
                subtext1: Color::Rgb(130, 181, 222),
                subtext0: Color::Rgb(97, 149, 195),
                overlay0: Color::Rgb(54, 93, 132),
                surface2: Color::Rgb(27, 54, 85),
                surface1: Color::Rgb(18, 41, 69),
                surface0: Color::Rgb(8, 22, 44),
                border: Color::Rgb(60, 110, 158),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sky,
                    categorical.teal,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.maroon,
                    categorical.red,
                    categorical.rosewater,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.sapphire,
                    medium: categorical.sky,
                    high: categorical.rosewater,
                    empty: Color::Rgb(16, 33, 58),
                },
                stream: ThemeStream {
                    inflow: categorical.sapphire,
                    outflow: categorical.sky,
                },
                categorical,
            },
        }
    }

    pub fn deep_sky() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(20, 66, 122),
            flamingo: Color::Rgb(32, 82, 142),
            pink: Color::Rgb(46, 102, 166),
            mauve: Color::Rgb(63, 124, 191),
            red: Color::Rgb(86, 149, 216),
            maroon: Color::Rgb(110, 171, 231),
            peach: Color::Rgb(136, 193, 244),
            yellow: Color::Rgb(163, 211, 250),
            green: Color::Rgb(190, 225, 252),
            teal: Color::Rgb(214, 236, 255),
            sky: Color::Rgb(230, 244, 255),
            sapphire: Color::Rgb(206, 229, 250),
            blue: Color::Rgb(180, 212, 242),
            lavender: Color::Rgb(152, 191, 230),
        };

        Self {
            name: ThemeName::DeepSky,
            effects: ThemeEffects {
                wave_enabled: true,
                wave_hz: 0.34,
                wave_intensity: 0.16,
                wave_wavelength: 56.0,
                wave_angle_degrees: -68.0,
                wave_mode: WaveMode::Linear,
                ..ThemeEffects::default()
            },
            semantic: ThemeSemantic {
                text: Color::Rgb(32, 78, 136),
                subtext1: Color::Rgb(55, 103, 161),
                subtext0: Color::Rgb(80, 126, 183),
                overlay0: Color::Rgb(116, 157, 208),
                surface2: Color::Rgb(170, 198, 226),
                surface1: Color::Rgb(214, 232, 247),
                surface0: Color::Rgb(202, 223, 241),
                border: Color::Rgb(104, 149, 201),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.blue,
                    categorical.sapphire,
                    categorical.sky,
                    categorical.teal,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.rosewater,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.blue,
                    medium: categorical.sky,
                    high: categorical.rosewater,
                    empty: Color::Rgb(192, 214, 235),
                },
                stream: ThemeStream {
                    inflow: categorical.sapphire,
                    outflow: categorical.sky,
                },
                categorical,
            },
        }
    }

    pub fn gold() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(255, 248, 226),
            flamingo: Color::Rgb(255, 234, 168),
            pink: Color::Rgb(255, 220, 120),
            mauve: Color::Rgb(243, 198, 83),
            red: Color::Rgb(224, 172, 53),
            maroon: Color::Rgb(221, 170, 58),
            peach: Color::Rgb(208, 154, 46),
            yellow: Color::Rgb(194, 139, 35),
            green: Color::Rgb(180, 125, 30),
            teal: Color::Rgb(166, 112, 26),
            sky: Color::Rgb(153, 101, 23),
            sapphire: Color::Rgb(141, 91, 21),
            blue: Color::Rgb(130, 82, 19),
            lavender: Color::Rgb(120, 74, 18),
        };

        Self {
            name: ThemeName::Gold,
            effects: ThemeEffects {
                local_enabled: true,
                flicker_hz: 10.0,
                flicker_intensity: 0.22,
                local_burst_duty: 0.10,
                local_burst_hz: 1.2,
                local_idle_intensity: 0.03,
                local_burst_boost: 1.45,
                wave_enabled: true,
                wave_hz: 0.75,
                wave_intensity: 0.18,
                wave_wavelength: 34.0,
                wave_angle_degrees: 22.0,
                wave_mode: WaveMode::Linear,
                particle: ThemeParticleEffect::default(),
            },
            semantic: ThemeSemantic {
                text: Color::Rgb(255, 236, 175),
                subtext1: Color::Rgb(248, 214, 142),
                subtext0: Color::Rgb(229, 186, 106),
                overlay0: Color::Rgb(168, 126, 58),
                surface2: Color::Rgb(92, 64, 27),
                surface1: Color::Rgb(74, 49, 20),
                surface0: Color::Rgb(54, 34, 13),
                border: Color::Rgb(210, 166, 79),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sky,
                    categorical.teal,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.maroon,
                    categorical.red,
                    categorical.rosewater,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.yellow,
                    medium: categorical.yellow,
                    high: categorical.yellow,
                    empty: Color::Rgb(78, 53, 22),
                },
                stream: ThemeStream {
                    inflow: categorical.peach,
                    outflow: categorical.flamingo,
                },
                categorical,
            },
        }
    }

    pub fn dracula() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(248, 248, 242),
            flamingo: Color::Rgb(255, 184, 108),
            pink: Color::Rgb(255, 121, 198),
            mauve: Color::Rgb(189, 147, 249),
            red: Color::Rgb(255, 85, 85),
            maroon: Color::Rgb(255, 110, 139),
            peach: Color::Rgb(255, 184, 108),
            yellow: Color::Rgb(241, 250, 140),
            green: Color::Rgb(80, 250, 123),
            teal: Color::Rgb(139, 233, 253),
            sky: Color::Rgb(139, 233, 253),
            sapphire: Color::Rgb(98, 114, 164),
            blue: Color::Rgb(139, 233, 253),
            lavender: Color::Rgb(189, 147, 249),
        };

        Self {
            name: ThemeName::Dracula,
            effects: ThemeEffects::default(),
            semantic: ThemeSemantic {
                text: Color::Rgb(248, 248, 242),
                subtext1: Color::Rgb(189, 147, 249),
                subtext0: Color::Rgb(98, 114, 164),
                overlay0: Color::Rgb(68, 71, 90),
                surface2: Color::Rgb(68, 71, 90),
                surface1: Color::Rgb(56, 59, 77),
                surface0: Color::Rgb(40, 42, 54),
                border: Color::Rgb(95, 100, 128),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sky,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.red,
                    categorical.pink,
                    categorical.mauve,
                    categorical.lavender,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.mauve,
                    medium: categorical.mauve,
                    high: categorical.mauve,
                    empty: Color::Rgb(68, 71, 90),
                },
                stream: ThemeStream {
                    inflow: categorical.blue,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn nord() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(236, 239, 244),
            flamingo: Color::Rgb(216, 222, 233),
            pink: Color::Rgb(191, 97, 106),
            mauve: Color::Rgb(180, 142, 173),
            red: Color::Rgb(191, 97, 106),
            maroon: Color::Rgb(208, 135, 112),
            peach: Color::Rgb(208, 135, 112),
            yellow: Color::Rgb(235, 203, 139),
            green: Color::Rgb(163, 190, 140),
            teal: Color::Rgb(143, 188, 187),
            sky: Color::Rgb(136, 192, 208),
            sapphire: Color::Rgb(129, 161, 193),
            blue: Color::Rgb(94, 129, 172),
            lavender: Color::Rgb(180, 142, 173),
        };

        Self {
            name: ThemeName::Nord,
            effects: ThemeEffects::default(),
            semantic: ThemeSemantic {
                text: Color::Rgb(236, 239, 244),
                subtext1: Color::Rgb(216, 222, 233),
                subtext0: Color::Rgb(143, 188, 187),
                overlay0: Color::Rgb(76, 86, 106),
                surface2: Color::Rgb(59, 66, 82),
                surface1: Color::Rgb(46, 52, 64),
                surface0: Color::Rgb(43, 48, 59),
                border: Color::Rgb(98, 112, 137),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sky,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.red,
                    categorical.mauve,
                    categorical.blue,
                    categorical.lavender,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.blue,
                    medium: categorical.sky,
                    high: categorical.green,
                    empty: Color::Rgb(46, 52, 64),
                },
                stream: ThemeStream {
                    inflow: categorical.blue,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn gruvbox_dark() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(235, 219, 178),
            flamingo: Color::Rgb(214, 93, 14),
            pink: Color::Rgb(211, 134, 155),
            mauve: Color::Rgb(211, 134, 155),
            red: Color::Rgb(251, 73, 52),
            maroon: Color::Rgb(204, 36, 29),
            peach: Color::Rgb(254, 128, 25),
            yellow: Color::Rgb(250, 189, 47),
            green: Color::Rgb(184, 187, 38),
            teal: Color::Rgb(142, 192, 124),
            sky: Color::Rgb(131, 165, 152),
            sapphire: Color::Rgb(69, 133, 136),
            blue: Color::Rgb(131, 165, 152),
            lavender: Color::Rgb(214, 93, 14),
        };

        Self {
            name: ThemeName::GruvboxDark,
            effects: ThemeEffects::default(),
            semantic: ThemeSemantic {
                text: Color::Rgb(235, 219, 178),
                subtext1: Color::Rgb(213, 196, 161),
                subtext0: Color::Rgb(168, 153, 132),
                overlay0: Color::Rgb(124, 111, 100),
                surface2: Color::Rgb(66, 61, 58),
                surface1: Color::Rgb(50, 48, 47),
                surface0: Color::Rgb(40, 40, 40),
                border: Color::Rgb(97, 88, 78),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.blue,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.red,
                    categorical.maroon,
                    categorical.mauve,
                    categorical.pink,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.yellow,
                    medium: categorical.yellow,
                    high: categorical.yellow,
                    empty: Color::Rgb(60, 56, 54),
                },
                stream: ThemeStream {
                    inflow: categorical.blue,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn tokyo_night() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(192, 202, 245),
            flamingo: Color::Rgb(255, 158, 100),
            pink: Color::Rgb(247, 118, 142),
            mauve: Color::Rgb(187, 154, 247),
            red: Color::Rgb(247, 118, 142),
            maroon: Color::Rgb(255, 158, 100),
            peach: Color::Rgb(255, 158, 100),
            yellow: Color::Rgb(224, 175, 104),
            green: Color::Rgb(158, 206, 106),
            teal: Color::Rgb(125, 207, 255),
            sky: Color::Rgb(125, 207, 255),
            sapphire: Color::Rgb(122, 162, 247),
            blue: Color::Rgb(122, 162, 247),
            lavender: Color::Rgb(187, 154, 247),
        };

        Self {
            name: ThemeName::TokyoNight,
            effects: ThemeEffects::default(),
            semantic: ThemeSemantic {
                text: Color::Rgb(192, 202, 245),
                subtext1: Color::Rgb(169, 177, 214),
                subtext0: Color::Rgb(113, 123, 174),
                overlay0: Color::Rgb(65, 72, 104),
                surface2: Color::Rgb(41, 46, 66),
                surface1: Color::Rgb(60, 65, 90),
                surface0: Color::Rgb(26, 27, 38),
                border: Color::Rgb(89, 98, 142),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sky,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.red,
                    categorical.mauve,
                    categorical.blue,
                    categorical.lavender,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.mauve,
                    medium: categorical.mauve,
                    high: categorical.mauve,
                    empty: Color::Rgb(36, 40, 59),
                },
                stream: ThemeStream {
                    inflow: categorical.blue,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn one_dark() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(171, 178, 191),
            flamingo: Color::Rgb(209, 154, 102),
            pink: Color::Rgb(198, 120, 221),
            mauve: Color::Rgb(198, 120, 221),
            red: Color::Rgb(224, 108, 117),
            maroon: Color::Rgb(190, 80, 70),
            peach: Color::Rgb(209, 154, 102),
            yellow: Color::Rgb(229, 192, 123),
            green: Color::Rgb(152, 195, 121),
            teal: Color::Rgb(86, 182, 194),
            sky: Color::Rgb(97, 175, 239),
            sapphire: Color::Rgb(97, 175, 239),
            blue: Color::Rgb(97, 175, 239),
            lavender: Color::Rgb(198, 120, 221),
        };

        Self {
            name: ThemeName::OneDark,
            effects: ThemeEffects::default(),
            semantic: ThemeSemantic {
                text: Color::Rgb(171, 178, 191),
                subtext1: Color::Rgb(146, 150, 165),
                subtext0: Color::Rgb(111, 119, 137),
                overlay0: Color::Rgb(73, 78, 90),
                surface2: Color::Rgb(47, 51, 61),
                surface1: Color::Rgb(65, 72, 80),
                surface0: Color::Rgb(30, 33, 39),
                border: Color::Rgb(97, 105, 121),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sky,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.red,
                    categorical.pink,
                    categorical.mauve,
                    categorical.lavender,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.blue,
                    medium: categorical.blue,
                    high: categorical.blue,
                    empty: Color::Rgb(40, 44, 52),
                },
                stream: ThemeStream {
                    inflow: categorical.blue,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn solarized_dark() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(131, 148, 150),
            flamingo: Color::Rgb(203, 75, 22),
            pink: Color::Rgb(211, 54, 130),
            mauve: Color::Rgb(108, 113, 196),
            red: Color::Rgb(220, 50, 47),
            maroon: Color::Rgb(203, 75, 22),
            peach: Color::Rgb(203, 75, 22),
            yellow: Color::Rgb(181, 137, 0),
            green: Color::Rgb(133, 153, 0),
            teal: Color::Rgb(42, 161, 152),
            sky: Color::Rgb(38, 139, 210),
            sapphire: Color::Rgb(38, 139, 210),
            blue: Color::Rgb(38, 139, 210),
            lavender: Color::Rgb(108, 113, 196),
        };

        Self {
            name: ThemeName::SolarizedDark,
            effects: ThemeEffects::default(),
            semantic: ThemeSemantic {
                text: Color::Rgb(131, 148, 150),
                subtext1: Color::Rgb(147, 161, 161),
                subtext0: Color::Rgb(101, 123, 131),
                overlay0: Color::Rgb(88, 110, 117),
                surface2: Color::Rgb(88, 110, 117),
                surface1: Color::Rgb(7, 54, 66),
                surface0: Color::Rgb(0, 43, 54),
                border: Color::Rgb(88, 110, 117),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sky,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.red,
                    categorical.pink,
                    categorical.mauve,
                    categorical.lavender,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.teal,
                    medium: categorical.teal,
                    high: categorical.teal,
                    empty: Color::Rgb(7, 54, 66),
                },
                stream: ThemeStream {
                    inflow: categorical.blue,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn monokai() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(248, 248, 242),
            flamingo: Color::Rgb(253, 151, 31),
            pink: Color::Rgb(249, 38, 114),
            mauve: Color::Rgb(174, 129, 255),
            red: Color::Rgb(249, 38, 114),
            maroon: Color::Rgb(204, 102, 119),
            peach: Color::Rgb(253, 151, 31),
            yellow: Color::Rgb(230, 219, 116),
            green: Color::Rgb(166, 226, 46),
            teal: Color::Rgb(102, 217, 239),
            sky: Color::Rgb(102, 217, 239),
            sapphire: Color::Rgb(117, 113, 94),
            blue: Color::Rgb(102, 217, 239),
            lavender: Color::Rgb(174, 129, 255),
        };

        Self {
            name: ThemeName::Monokai,
            effects: ThemeEffects::default(),
            semantic: ThemeSemantic {
                text: Color::Rgb(248, 248, 242),
                subtext1: Color::Rgb(174, 129, 255),
                subtext0: Color::Rgb(117, 113, 94),
                overlay0: Color::Rgb(73, 72, 62),
                surface2: Color::Rgb(49, 50, 43),
                surface1: Color::Rgb(70, 72, 65),
                surface0: Color::Rgb(27, 28, 24),
                border: Color::Rgb(96, 96, 82),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sky,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.red,
                    categorical.pink,
                    categorical.mauve,
                    categorical.lavender,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.green,
                    medium: categorical.green,
                    high: categorical.green,
                    empty: Color::Rgb(39, 40, 34),
                },
                stream: ThemeStream {
                    inflow: categorical.blue,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn everforest_dark() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(211, 198, 170),
            flamingo: Color::Rgb(230, 126, 128),
            pink: Color::Rgb(231, 138, 131),
            mauve: Color::Rgb(215, 153, 33),
            red: Color::Rgb(230, 126, 128),
            maroon: Color::Rgb(229, 152, 117),
            peach: Color::Rgb(229, 152, 117),
            yellow: Color::Rgb(219, 188, 127),
            green: Color::Rgb(167, 192, 128),
            teal: Color::Rgb(131, 192, 146),
            sky: Color::Rgb(127, 187, 179),
            sapphire: Color::Rgb(115, 163, 145),
            blue: Color::Rgb(127, 187, 179),
            lavender: Color::Rgb(214, 153, 182),
        };

        Self {
            name: ThemeName::EverforestDark,
            effects: ThemeEffects::default(),
            semantic: ThemeSemantic {
                text: Color::Rgb(211, 198, 170),
                subtext1: Color::Rgb(167, 192, 128),
                subtext0: Color::Rgb(133, 147, 138),
                overlay0: Color::Rgb(94, 100, 104),
                surface2: Color::Rgb(59, 69, 71),
                surface1: Color::Rgb(47, 56, 58),
                surface0: Color::Rgb(43, 51, 57),
                border: Color::Rgb(84, 101, 103),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sky,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.red,
                    categorical.pink,
                    categorical.mauve,
                    categorical.lavender,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.green,
                    medium: categorical.green,
                    high: categorical.green,
                    empty: Color::Rgb(59, 69, 71),
                },
                stream: ThemeStream {
                    inflow: categorical.blue,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn kanagawa() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(220, 215, 186),
            flamingo: Color::Rgb(210, 126, 154),
            pink: Color::Rgb(210, 126, 154),
            mauve: Color::Rgb(149, 127, 184),
            red: Color::Rgb(195, 64, 67),
            maroon: Color::Rgb(195, 64, 67),
            peach: Color::Rgb(255, 160, 102),
            yellow: Color::Rgb(192, 163, 110),
            green: Color::Rgb(118, 148, 106),
            teal: Color::Rgb(106, 149, 137),
            sky: Color::Rgb(126, 156, 216),
            sapphire: Color::Rgb(101, 133, 153),
            blue: Color::Rgb(126, 156, 216),
            lavender: Color::Rgb(149, 127, 184),
        };

        Self {
            name: ThemeName::Kanagawa,
            effects: ThemeEffects::default(),
            semantic: ThemeSemantic {
                text: Color::Rgb(220, 215, 186),
                subtext1: Color::Rgb(166, 173, 200),
                subtext0: Color::Rgb(114, 113, 133),
                overlay0: Color::Rgb(84, 84, 111),
                surface2: Color::Rgb(54, 54, 75),
                surface1: Color::Rgb(42, 42, 62),
                surface0: Color::Rgb(31, 31, 40),
                border: Color::Rgb(84, 84, 111),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sky,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.red,
                    categorical.pink,
                    categorical.mauve,
                    categorical.lavender,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.sapphire,
                    medium: categorical.sapphire,
                    high: categorical.sapphire,
                    empty: Color::Rgb(54, 54, 75),
                },
                stream: ThemeStream {
                    inflow: categorical.blue,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn github_dark() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(201, 209, 217),
            flamingo: Color::Rgb(255, 122, 127),
            pink: Color::Rgb(210, 153, 255),
            mauve: Color::Rgb(188, 140, 255),
            red: Color::Rgb(248, 81, 73),
            maroon: Color::Rgb(255, 123, 114),
            peach: Color::Rgb(255, 166, 87),
            yellow: Color::Rgb(210, 153, 34),
            green: Color::Rgb(63, 185, 80),
            teal: Color::Rgb(57, 197, 207),
            sky: Color::Rgb(103, 193, 255),
            sapphire: Color::Rgb(88, 166, 255),
            blue: Color::Rgb(88, 166, 255),
            lavender: Color::Rgb(188, 140, 255),
        };

        Self {
            name: ThemeName::GitHubDark,
            effects: ThemeEffects::default(),
            semantic: ThemeSemantic {
                text: Color::Rgb(201, 209, 217),
                subtext1: Color::Rgb(139, 148, 158),
                subtext0: Color::Rgb(110, 118, 129),
                overlay0: Color::Rgb(48, 54, 61),
                surface2: Color::Rgb(33, 38, 45),
                surface1: Color::Rgb(50, 60, 70),
                surface0: Color::Rgb(13, 17, 23),
                border: Color::Rgb(70, 79, 90),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sky,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.red,
                    categorical.pink,
                    categorical.mauve,
                    categorical.lavender,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.blue,
                    medium: categorical.blue,
                    high: categorical.blue,
                    empty: Color::Rgb(33, 38, 45),
                },
                stream: ThemeStream {
                    inflow: categorical.blue,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn solarized_light() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(101, 123, 131),
            flamingo: Color::Rgb(203, 75, 22),
            pink: Color::Rgb(211, 54, 130),
            mauve: Color::Rgb(108, 113, 196),
            red: Color::Rgb(220, 50, 47),
            maroon: Color::Rgb(203, 75, 22),
            peach: Color::Rgb(203, 75, 22),
            yellow: Color::Rgb(181, 137, 0),
            green: Color::Rgb(133, 153, 0),
            teal: Color::Rgb(42, 161, 152),
            sky: Color::Rgb(38, 139, 210),
            sapphire: Color::Rgb(38, 139, 210),
            blue: Color::Rgb(38, 139, 210),
            lavender: Color::Rgb(108, 113, 196),
        };

        Self {
            name: ThemeName::SolarizedLight,
            effects: ThemeEffects::default(),
            semantic: ThemeSemantic {
                text: Color::Rgb(88, 110, 117),
                subtext1: Color::Rgb(101, 123, 131),
                subtext0: Color::Rgb(101, 123, 131),
                overlay0: Color::Rgb(147, 161, 161),
                surface2: Color::Rgb(147, 161, 161),
                surface1: Color::Rgb(238, 232, 213),
                surface0: Color::Rgb(253, 246, 227),
                border: Color::Rgb(147, 161, 161),
                white: Color::Black,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sky,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.red,
                    categorical.pink,
                    categorical.mauve,
                    categorical.lavender,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.blue,
                    medium: categorical.blue,
                    high: categorical.blue,
                    empty: Color::Rgb(238, 232, 213),
                },
                stream: ThemeStream {
                    inflow: categorical.blue,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn matrix() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(0, 255, 65),
            flamingo: Color::Rgb(0, 143, 17),
            pink: Color::Rgb(0, 255, 65),
            mauve: Color::Rgb(0, 59, 0),
            red: Color::Rgb(255, 95, 95),
            maroon: Color::Rgb(0, 143, 17),
            peach: Color::Rgb(0, 255, 65),
            yellow: Color::Rgb(255, 240, 120),
            green: Color::Rgb(0, 255, 65),
            teal: Color::Rgb(0, 255, 65),
            sky: Color::Rgb(0, 255, 65),
            sapphire: Color::Rgb(0, 255, 65),
            blue: Color::Rgb(0, 255, 65),
            lavender: Color::Rgb(0, 255, 65),
        };

        Self {
            name: ThemeName::Matrix,
            effects: ThemeEffects {
                local_enabled: true,
                flicker_hz: 5.0,
                flicker_intensity: 0.16,
                particle: ThemeParticleEffect {
                    enabled: true,
                    layer_mode: ParticleLayerMode::Background,
                    profile: ParticleProfile::Matrix,
                    density: 0.026,
                    speed: 0.82,
                    intensity: 0.62,
                },
                ..ThemeEffects::default()
            },
            semantic: ThemeSemantic {
                text: Color::Rgb(0, 255, 65),
                subtext1: Color::Rgb(0, 204, 52),
                subtext0: Color::Rgb(0, 143, 17),
                overlay0: Color::Rgb(0, 89, 11),
                surface2: Color::Rgb(0, 59, 0),
                surface1: Color::Rgb(0, 180, 0),
                surface0: Color::Rgb(0, 0, 0),
                border: Color::Rgb(0, 143, 17),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    Color::Rgb(0, 59, 0),
                    Color::Rgb(0, 89, 11),
                    Color::Rgb(0, 143, 17),
                    Color::Rgb(0, 204, 52),
                    Color::Rgb(0, 255, 65),
                    Color::Rgb(102, 255, 102),
                    Color::Rgb(153, 255, 153),
                    Color::Rgb(204, 255, 204),
                ],
                ip_hash: [
                    Color::Rgb(0, 255, 65),
                    Color::Rgb(0, 204, 52),
                    Color::Rgb(0, 143, 17),
                    Color::Rgb(0, 89, 11),
                    Color::Rgb(0, 59, 0),
                    Color::Rgb(0, 255, 65),
                    Color::Rgb(0, 204, 52),
                    Color::Rgb(0, 143, 17),
                    Color::Rgb(0, 89, 11),
                    Color::Rgb(0, 59, 0),
                    Color::Rgb(0, 255, 65),
                    Color::Rgb(0, 204, 52),
                    Color::Rgb(0, 143, 17),
                    Color::Rgb(0, 89, 11),
                ],
                heatmap: ThemeHeatmap {
                    low: Color::Rgb(0, 59, 0),
                    medium: Color::Rgb(0, 143, 17),
                    high: Color::Rgb(0, 255, 65),
                    empty: Color::Rgb(0, 20, 0),
                },
                stream: ThemeStream {
                    inflow: Color::Rgb(0, 255, 65),
                    outflow: Color::Rgb(0, 143, 17),
                },
                categorical,
            },
        }
    }

    pub fn catppuccin_latte() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(220, 138, 120),
            flamingo: Color::Rgb(221, 120, 120),
            pink: Color::Rgb(234, 118, 203),
            mauve: Color::Rgb(136, 57, 239),
            red: Color::Rgb(210, 15, 57),
            maroon: Color::Rgb(230, 69, 83),
            peach: Color::Rgb(254, 100, 11),
            yellow: Color::Rgb(223, 142, 29),
            green: Color::Rgb(64, 160, 43),
            teal: Color::Rgb(23, 146, 153),
            sky: Color::Rgb(4, 165, 229),
            sapphire: Color::Rgb(32, 159, 181),
            blue: Color::Rgb(30, 102, 245),
            lavender: Color::Rgb(114, 135, 253),
        };

        Self {
            name: ThemeName::CatppuccinLatte,
            effects: ThemeEffects::default(),
            semantic: ThemeSemantic {
                text: Color::Rgb(76, 79, 105),
                subtext1: Color::Rgb(92, 95, 119),
                subtext0: Color::Rgb(108, 111, 133),
                overlay0: Color::Rgb(156, 160, 176),
                surface2: Color::Rgb(172, 176, 190),
                surface1: Color::Rgb(188, 192, 204),
                surface0: Color::Rgb(204, 208, 218),
                border: Color::Rgb(130, 134, 151),
                white: Color::Black,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sky,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.maroon,
                    categorical.red,
                    categorical.flamingo,
                    categorical.pink,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.mauve,
                    medium: categorical.mauve,
                    high: categorical.mauve,
                    empty: Color::Rgb(188, 192, 204),
                },
                stream: ThemeStream {
                    inflow: categorical.blue,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn cyberpunk() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(255, 255, 255),
            flamingo: Color::Rgb(255, 0, 255),
            pink: Color::Rgb(255, 0, 255),
            mauve: Color::Rgb(150, 0, 255),
            red: Color::Rgb(255, 0, 60),
            maroon: Color::Rgb(255, 0, 100),
            peach: Color::Rgb(255, 100, 0),
            yellow: Color::Rgb(253, 245, 0),
            green: Color::Rgb(0, 255, 159),
            teal: Color::Rgb(0, 255, 255),
            sky: Color::Rgb(0, 184, 255),
            sapphire: Color::Rgb(0, 114, 255),
            blue: Color::Rgb(5, 217, 255),
            lavender: Color::Rgb(150, 0, 255),
        };

        Self {
            name: ThemeName::Cyberpunk,
            effects: ThemeEffects {
                local_enabled: true,
                flicker_hz: 9.0,
                flicker_intensity: 0.26,
                ..ThemeEffects::default()
            },
            semantic: ThemeSemantic {
                text: Color::Rgb(253, 245, 0),
                subtext1: Color::Rgb(0, 255, 255),
                subtext0: Color::Rgb(255, 0, 255),
                overlay0: Color::Rgb(50, 0, 100),
                surface2: Color::Rgb(45, 8, 82),
                surface1: Color::Rgb(120, 50, 160),
                surface0: Color::Rgb(0, 0, 0),
                border: Color::Rgb(255, 0, 255),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.blue,
                    categorical.teal,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.red,
                    categorical.pink,
                    categorical.mauve,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.pink,
                    medium: categorical.pink,
                    high: categorical.pink,
                    empty: Color::Rgb(30, 0, 60),
                },
                stream: ThemeStream {
                    inflow: categorical.blue,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn ayu_dark() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(230, 225, 207),
            flamingo: Color::Rgb(255, 180, 84),
            pink: Color::Rgb(240, 117, 181),
            mauve: Color::Rgb(223, 177, 242),
            red: Color::Rgb(255, 51, 51),
            maroon: Color::Rgb(242, 121, 131),
            peach: Color::Rgb(255, 180, 84),
            yellow: Color::Rgb(242, 151, 24),
            green: Color::Rgb(184, 204, 82),
            teal: Color::Rgb(149, 230, 203),
            sky: Color::Rgb(54, 163, 217),
            sapphire: Color::Rgb(54, 163, 217),
            blue: Color::Rgb(54, 163, 217),
            lavender: Color::Rgb(223, 177, 242),
        };

        Self {
            name: ThemeName::AyuDark,
            effects: ThemeEffects::default(),
            semantic: ThemeSemantic {
                text: Color::Rgb(230, 225, 207),
                subtext1: Color::Rgb(171, 176, 191),
                subtext0: Color::Rgb(92, 103, 115),
                overlay0: Color::Rgb(62, 71, 82),
                surface2: Color::Rgb(33, 39, 47),
                surface1: Color::Rgb(55, 65, 75),
                surface0: Color::Rgb(15, 20, 25),
                border: Color::Rgb(84, 96, 112),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sky,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.red,
                    categorical.pink,
                    categorical.mauve,
                    categorical.lavender,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.teal,
                    medium: categorical.teal,
                    high: categorical.teal,
                    empty: Color::Rgb(25, 30, 36),
                },
                stream: ThemeStream {
                    inflow: categorical.blue,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn zenburn() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(220, 220, 204),
            flamingo: Color::Rgb(223, 175, 143),
            pink: Color::Rgb(220, 140, 195),
            mauve: Color::Rgb(156, 144, 186),
            red: Color::Rgb(204, 147, 147),
            maroon: Color::Rgb(188, 131, 121),
            peach: Color::Rgb(223, 175, 143),
            yellow: Color::Rgb(240, 223, 175),
            green: Color::Rgb(127, 159, 127),
            teal: Color::Rgb(147, 177, 187),
            sky: Color::Rgb(140, 208, 211),
            sapphire: Color::Rgb(115, 139, 140),
            blue: Color::Rgb(140, 208, 211),
            lavender: Color::Rgb(156, 144, 186),
        };

        Self {
            name: ThemeName::Zenburn,
            effects: ThemeEffects::default(),
            semantic: ThemeSemantic {
                text: Color::Rgb(220, 220, 204),
                subtext1: Color::Rgb(159, 159, 159),
                subtext0: Color::Rgb(127, 159, 127),
                overlay0: Color::Rgb(83, 83, 83),
                surface2: Color::Rgb(71, 71, 71),
                surface1: Color::Rgb(63, 63, 63),
                surface0: Color::Rgb(50, 50, 50),
                border: Color::Rgb(105, 105, 105),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sky,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.red,
                    categorical.pink,
                    categorical.mauve,
                    categorical.lavender,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.green,
                    medium: categorical.green,
                    high: categorical.green,
                    empty: Color::Rgb(71, 71, 71),
                },
                stream: ThemeStream {
                    inflow: categorical.blue,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn synthwave_84() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(255, 255, 255),
            flamingo: Color::Rgb(255, 126, 219),
            pink: Color::Rgb(255, 126, 219),
            mauve: Color::Rgb(114, 241, 184),
            red: Color::Rgb(249, 126, 114),
            maroon: Color::Rgb(249, 126, 114),
            peach: Color::Rgb(254, 238, 0),
            yellow: Color::Rgb(254, 238, 0),
            green: Color::Rgb(114, 241, 184),
            teal: Color::Rgb(54, 249, 246),
            sky: Color::Rgb(54, 249, 246),
            sapphire: Color::Rgb(54, 249, 246),
            blue: Color::Rgb(54, 249, 246),
            lavender: Color::Rgb(114, 241, 184),
        };

        Self {
            name: ThemeName::Synthwave84,
            effects: ThemeEffects {
                wave_enabled: true,
                wave_hz: 0.85,
                wave_intensity: 0.11,
                wave_wavelength: 30.0,
                wave_mode: WaveMode::RadialOut,
                ..ThemeEffects::default()
            },
            semantic: ThemeSemantic {
                text: Color::Rgb(249, 126, 114),
                subtext1: Color::Rgb(255, 126, 219),
                subtext0: Color::Rgb(54, 249, 246),
                overlay0: Color::Rgb(103, 78, 131),
                surface2: Color::Rgb(65, 55, 90),
                surface1: Color::Rgb(70, 50, 90),
                surface0: Color::Rgb(36, 27, 47),
                border: Color::Rgb(255, 126, 219),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.teal,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.red,
                    categorical.pink,
                    categorical.mauve,
                    categorical.rosewater,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.pink,
                    medium: categorical.pink,
                    high: categorical.pink,
                    empty: Color::Rgb(52, 43, 73),
                },
                stream: ThemeStream {
                    inflow: categorical.blue,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn github_light() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(36, 41, 47),
            flamingo: Color::Rgb(207, 34, 46),
            pink: Color::Rgb(130, 80, 223),
            mauve: Color::Rgb(130, 80, 223),
            red: Color::Rgb(207, 34, 46),
            maroon: Color::Rgb(207, 34, 46),
            peach: Color::Rgb(154, 103, 0),
            yellow: Color::Rgb(154, 103, 0),
            green: Color::Rgb(26, 127, 55),
            teal: Color::Rgb(5, 153, 112),
            sky: Color::Rgb(5, 153, 112),
            sapphire: Color::Rgb(9, 105, 218),
            blue: Color::Rgb(9, 105, 218),
            lavender: Color::Rgb(130, 80, 223),
        };

        Self {
            name: ThemeName::GitHubLight,
            effects: ThemeEffects::default(),
            semantic: ThemeSemantic {
                text: Color::Rgb(36, 41, 47),
                subtext1: Color::Rgb(87, 96, 106),
                subtext0: Color::Rgb(101, 109, 118),
                overlay0: Color::Rgb(208, 215, 222),
                surface2: Color::Rgb(220, 226, 233),
                surface1: Color::Rgb(246, 248, 250),
                surface0: Color::Rgb(255, 255, 255),
                border: Color::Rgb(163, 173, 185),
                white: Color::Black,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sapphire,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.red,
                    categorical.pink,
                    categorical.mauve,
                    categorical.lavender,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.blue,
                    medium: categorical.blue,
                    high: categorical.blue,
                    empty: Color::Rgb(234, 238, 242),
                },
                stream: ThemeStream {
                    inflow: categorical.blue,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn vesper() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(255, 255, 255),
            flamingo: Color::Rgb(255, 128, 0),
            pink: Color::Rgb(255, 175, 0),
            mauve: Color::Rgb(160, 160, 160),
            red: Color::Rgb(255, 128, 0),
            maroon: Color::Rgb(255, 128, 0),
            peach: Color::Rgb(255, 175, 0),
            yellow: Color::Rgb(255, 175, 0),
            green: Color::Rgb(160, 160, 160),
            teal: Color::Rgb(160, 160, 160),
            sky: Color::Rgb(160, 160, 160),
            sapphire: Color::Rgb(160, 160, 160),
            blue: Color::Rgb(160, 160, 160),
            lavender: Color::Rgb(160, 160, 160),
        };

        Self {
            name: ThemeName::Vesper,
            effects: ThemeEffects::default(),
            semantic: ThemeSemantic {
                text: Color::Rgb(255, 255, 255),
                subtext1: Color::Rgb(160, 160, 160),
                subtext0: Color::Rgb(110, 110, 110),
                overlay0: Color::Rgb(70, 70, 70),
                surface2: Color::Rgb(40, 40, 40),
                surface1: Color::Rgb(60, 60, 60),
                surface0: Color::Rgb(16, 16, 16),
                border: Color::Rgb(120, 120, 120),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    Color::Rgb(80, 80, 80),
                    Color::Rgb(110, 110, 110),
                    Color::Rgb(140, 140, 140),
                    Color::Rgb(160, 160, 160),
                    Color::Rgb(180, 180, 180),
                    Color::Rgb(255, 175, 0),
                    Color::Rgb(255, 128, 0),
                    Color::Rgb(255, 255, 255),
                ],
                ip_hash: [
                    Color::Rgb(255, 255, 255),
                    Color::Rgb(255, 128, 0),
                    Color::Rgb(255, 175, 0),
                    Color::Rgb(160, 160, 160),
                    Color::Rgb(110, 110, 110),
                    Color::Rgb(255, 255, 255),
                    Color::Rgb(255, 128, 0),
                    Color::Rgb(255, 175, 0),
                    Color::Rgb(160, 160, 160),
                    Color::Rgb(110, 110, 110),
                    Color::Rgb(255, 255, 255),
                    Color::Rgb(255, 128, 0),
                    Color::Rgb(255, 175, 0),
                    Color::Rgb(160, 160, 160),
                ],
                heatmap: ThemeHeatmap {
                    low: categorical.peach,
                    medium: categorical.peach,
                    high: categorical.peach,
                    empty: Color::Rgb(30, 30, 30),
                },
                stream: ThemeStream {
                    inflow: Color::Rgb(255, 175, 0),
                    outflow: Color::Rgb(255, 128, 0),
                },
                categorical,
            },
        }
    }

    pub fn material_ocean() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(143, 147, 162),
            flamingo: Color::Rgb(255, 83, 112),
            pink: Color::Rgb(240, 113, 120),
            mauve: Color::Rgb(199, 146, 234),
            red: Color::Rgb(240, 113, 120),
            maroon: Color::Rgb(240, 113, 120),
            peach: Color::Rgb(247, 140, 108),
            yellow: Color::Rgb(255, 203, 107),
            green: Color::Rgb(195, 232, 141),
            teal: Color::Rgb(137, 221, 255),
            sky: Color::Rgb(137, 221, 255),
            sapphire: Color::Rgb(130, 170, 255),
            blue: Color::Rgb(130, 170, 255),
            lavender: Color::Rgb(199, 146, 234),
        };

        Self {
            name: ThemeName::MaterialOcean,
            effects: ThemeEffects::default(),
            semantic: ThemeSemantic {
                text: Color::Rgb(143, 147, 162),
                subtext1: Color::Rgb(113, 123, 145),
                subtext0: Color::Rgb(105, 114, 138),
                overlay0: Color::Rgb(53, 57, 74),
                surface2: Color::Rgb(37, 41, 58),
                surface1: Color::Rgb(45, 50, 75),
                surface0: Color::Rgb(15, 17, 26),
                border: Color::Rgb(100, 110, 140),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sky,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.red,
                    categorical.pink,
                    categorical.mauve,
                    categorical.lavender,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.teal,
                    medium: categorical.teal,
                    high: categorical.teal,
                    empty: Color::Rgb(25, 27, 41),
                },
                stream: ThemeStream {
                    inflow: categorical.blue,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn gruvbox_light() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(60, 56, 54),
            flamingo: Color::Rgb(175, 58, 3),
            pink: Color::Rgb(143, 63, 113),
            mauve: Color::Rgb(143, 63, 113),
            red: Color::Rgb(157, 0, 6),
            maroon: Color::Rgb(157, 0, 6),
            peach: Color::Rgb(175, 58, 3),
            yellow: Color::Rgb(181, 118, 20),
            green: Color::Rgb(121, 116, 14),
            teal: Color::Rgb(66, 123, 88),
            sky: Color::Rgb(7, 102, 120),
            sapphire: Color::Rgb(7, 102, 120),
            blue: Color::Rgb(7, 102, 120),
            lavender: Color::Rgb(143, 63, 113),
        };

        Self {
            name: ThemeName::GruvboxLight,
            effects: ThemeEffects::default(),
            semantic: ThemeSemantic {
                text: Color::Rgb(60, 56, 54),
                subtext1: Color::Rgb(80, 73, 69),
                subtext0: Color::Rgb(102, 92, 84),
                overlay0: Color::Rgb(146, 131, 116),
                surface2: Color::Rgb(213, 196, 161),
                surface1: Color::Rgb(200, 185, 155),
                surface0: Color::Rgb(251, 241, 199),
                border: Color::Rgb(173, 154, 132),
                white: Color::Black,
            },
            scale: ThemeScale {
                speed: [
                    categorical.blue,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.red,
                    categorical.maroon,
                    categorical.mauve,
                    categorical.pink,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.yellow,
                    medium: categorical.yellow,
                    high: categorical.yellow,
                    empty: Color::Rgb(213, 196, 161),
                },
                stream: ThemeStream {
                    inflow: categorical.blue,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn oxocarbon() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(255, 255, 255),
            flamingo: Color::Rgb(255, 126, 182),
            pink: Color::Rgb(255, 126, 182),
            mauve: Color::Rgb(190, 149, 255),
            red: Color::Rgb(238, 83, 103),
            maroon: Color::Rgb(238, 83, 103),
            peach: Color::Rgb(255, 169, 123),
            yellow: Color::Rgb(255, 233, 123),
            green: Color::Rgb(66, 190, 101),
            teal: Color::Rgb(51, 177, 255),
            sky: Color::Rgb(130, 207, 255),
            sapphire: Color::Rgb(130, 207, 255),
            blue: Color::Rgb(130, 207, 255),
            lavender: Color::Rgb(190, 149, 255),
        };

        Self {
            name: ThemeName::Oxocarbon,
            effects: ThemeEffects::default(),
            semantic: ThemeSemantic {
                text: Color::Rgb(255, 255, 255),
                subtext1: Color::Rgb(221, 221, 221),
                subtext0: Color::Rgb(171, 171, 171),
                overlay0: Color::Rgb(82, 82, 82),
                surface2: Color::Rgb(57, 57, 57),
                surface1: Color::Rgb(50, 55, 65),
                surface0: Color::Rgb(22, 22, 22),
                border: Color::Rgb(110, 110, 110),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sky,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.red,
                    categorical.pink,
                    categorical.mauve,
                    categorical.lavender,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.blue,
                    medium: categorical.blue,
                    high: categorical.blue,
                    empty: Color::Rgb(38, 38, 38),
                },
                stream: ThemeStream {
                    inflow: categorical.blue,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn rainbow() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(255, 0, 0),
            flamingo: Color::Rgb(255, 127, 0),
            pink: Color::Rgb(255, 0, 255),
            mauve: Color::Rgb(127, 0, 255),
            red: Color::Rgb(255, 0, 0),
            maroon: Color::Rgb(127, 0, 0),
            peach: Color::Rgb(255, 127, 0),
            yellow: Color::Rgb(255, 255, 0),
            green: Color::Rgb(0, 255, 0),
            teal: Color::Rgb(0, 255, 255),
            sky: Color::Rgb(0, 127, 255),
            sapphire: Color::Rgb(0, 0, 255),
            blue: Color::Rgb(0, 0, 255),
            lavender: Color::Rgb(127, 0, 255),
        };

        Self {
            name: ThemeName::Rainbow,
            effects: ThemeEffects {
                local_enabled: true,
                flicker_hz: 2.0,
                flicker_intensity: 0.1,
                ..ThemeEffects::default()
            },
            semantic: ThemeSemantic {
                text: Color::Rgb(242, 246, 252),
                subtext1: Color::Rgb(208, 217, 235),
                subtext0: Color::Rgb(171, 184, 209),
                overlay0: Color::Rgb(100, 119, 154),
                surface2: Color::Rgb(49, 64, 92),
                surface1: Color::Rgb(36, 49, 74),
                surface0: Color::Rgb(20, 30, 48),
                border: Color::Rgb(127, 149, 189),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    Color::Rgb(255, 0, 0),
                    Color::Rgb(255, 127, 0),
                    Color::Rgb(255, 255, 0),
                    Color::Rgb(0, 255, 0),
                    Color::Rgb(0, 255, 255),
                    Color::Rgb(0, 0, 255),
                    Color::Rgb(127, 0, 255),
                    Color::Rgb(255, 0, 255),
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.sapphire,
                    medium: categorical.sapphire,
                    high: categorical.pink,
                    empty: Color::Rgb(50, 50, 50),
                },
                stream: ThemeStream {
                    inflow: Color::Rgb(0, 255, 255),
                    outflow: Color::Rgb(255, 0, 255),
                },
                categorical,
            },
        }
    }

    pub fn inferno() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(255, 255, 255),
            flamingo: Color::Rgb(255, 100, 0),
            pink: Color::Rgb(255, 50, 0),
            mauve: Color::Rgb(150, 0, 0),
            red: Color::Rgb(255, 0, 0),
            maroon: Color::Rgb(150, 0, 0),
            peach: Color::Rgb(255, 150, 0),
            yellow: Color::Rgb(255, 255, 0),
            green: Color::Rgb(255, 200, 0),
            teal: Color::Rgb(255, 220, 100),
            sky: Color::Rgb(255, 240, 150),
            sapphire: Color::Rgb(255, 255, 200),
            blue: Color::Rgb(255, 255, 200),
            lavender: Color::Rgb(150, 0, 0),
        };

        Self {
            name: ThemeName::Inferno,
            effects: ThemeEffects {
                local_enabled: true,
                flicker_hz: 10.0,
                flicker_intensity: 0.30,
                ..ThemeEffects::default()
            },
            semantic: ThemeSemantic {
                text: Color::Rgb(255, 200, 0),
                subtext1: Color::Rgb(255, 150, 0),
                subtext0: Color::Rgb(255, 100, 0),
                overlay0: Color::Rgb(150, 50, 0),
                surface2: Color::Rgb(80, 20, 0),
                surface1: Color::Rgb(100, 40, 20),
                surface0: Color::Rgb(20, 0, 0),
                border: Color::Rgb(255, 50, 0),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    Color::Rgb(80, 0, 0),
                    Color::Rgb(150, 0, 0),
                    Color::Rgb(200, 50, 0),
                    Color::Rgb(255, 80, 0),
                    Color::Rgb(255, 120, 0),
                    Color::Rgb(255, 180, 0),
                    Color::Rgb(255, 220, 0),
                    Color::Rgb(255, 255, 100),
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.maroon,
                    medium: categorical.maroon,
                    high: categorical.yellow,
                    empty: Color::Rgb(40, 10, 0),
                },
                stream: ThemeStream {
                    inflow: Color::Rgb(255, 255, 0),
                    outflow: Color::Rgb(255, 50, 0),
                },
                categorical,
            },
        }
    }

    pub fn aurora() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(200, 255, 200),
            flamingo: Color::Rgb(150, 255, 150),
            pink: Color::Rgb(255, 150, 255),
            mauve: Color::Rgb(200, 150, 255),
            red: Color::Rgb(100, 255, 100),
            maroon: Color::Rgb(50, 200, 150),
            peach: Color::Rgb(100, 200, 255),
            yellow: Color::Rgb(150, 255, 255),
            green: Color::Rgb(0, 255, 128),
            teal: Color::Rgb(0, 255, 255),
            sky: Color::Rgb(128, 255, 255),
            sapphire: Color::Rgb(128, 128, 255),
            blue: Color::Rgb(150, 150, 255),
            lavender: Color::Rgb(200, 150, 255),
        };

        Self {
            name: ThemeName::Aurora,
            effects: ThemeEffects {
                local_enabled: true,
                flicker_hz: 3.0,
                flicker_intensity: 0.28,
                wave_enabled: true,
                wave_hz: 0.65,
                wave_intensity: 0.12,
                wave_wavelength: 34.0,
                wave_angle_degrees: 45.0,
                ..ThemeEffects::default()
            },
            semantic: ThemeSemantic {
                text: Color::Rgb(150, 255, 200),
                subtext1: Color::Rgb(100, 200, 255),
                subtext0: Color::Rgb(150, 150, 255),
                overlay0: Color::Rgb(40, 60, 100),
                surface2: Color::Rgb(20, 30, 60),
                surface1: Color::Rgb(30, 45, 90),
                surface0: Color::Rgb(5, 5, 25),
                border: Color::Rgb(0, 255, 128),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    Color::Rgb(5, 5, 40),
                    Color::Rgb(20, 40, 100),
                    Color::Rgb(40, 80, 150),
                    Color::Rgb(0, 150, 150),
                    Color::Rgb(0, 200, 100),
                    Color::Rgb(0, 255, 128),
                    Color::Rgb(100, 255, 200),
                    Color::Rgb(200, 255, 255),
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.sapphire,
                    medium: categorical.sapphire,
                    high: categorical.teal,
                    empty: Color::Rgb(20, 30, 60),
                },
                stream: ThemeStream {
                    inflow: Color::Rgb(0, 255, 255),
                    outflow: Color::Rgb(200, 150, 255),
                },
                categorical,
            },
        }
    }

    pub fn andromeda() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(255, 255, 255),
            flamingo: Color::Rgb(255, 74, 133),
            pink: Color::Rgb(255, 74, 133),
            mauve: Color::Rgb(173, 112, 255),
            red: Color::Rgb(255, 76, 110),
            maroon: Color::Rgb(255, 76, 110),
            peach: Color::Rgb(255, 202, 125),
            yellow: Color::Rgb(255, 230, 109),
            green: Color::Rgb(0, 230, 152),
            teal: Color::Rgb(0, 230, 230),
            sky: Color::Rgb(0, 150, 255),
            sapphire: Color::Rgb(0, 150, 255),
            blue: Color::Rgb(0, 150, 255),
            lavender: Color::Rgb(173, 112, 255),
        };

        Self {
            name: ThemeName::Andromeda,
            effects: ThemeEffects::default(),
            semantic: ThemeSemantic {
                text: Color::Rgb(213, 218, 227),
                subtext1: Color::Rgb(153, 158, 167),
                subtext0: Color::Rgb(116, 123, 136),
                overlay0: Color::Rgb(59, 64, 72),
                surface2: Color::Rgb(43, 48, 59),
                surface1: Color::Rgb(65, 70, 80),
                surface0: Color::Rgb(29, 32, 38),
                border: Color::Rgb(79, 86, 100),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sky,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.red,
                    categorical.pink,
                    categorical.mauve,
                    categorical.lavender,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.mauve,
                    medium: categorical.mauve,
                    high: categorical.mauve,
                    empty: Color::Rgb(43, 48, 59),
                },
                stream: ThemeStream {
                    inflow: categorical.blue,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn rose_pine() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(224, 222, 244),
            flamingo: Color::Rgb(246, 193, 119),
            pink: Color::Rgb(235, 111, 146),
            mauve: Color::Rgb(196, 167, 231),
            red: Color::Rgb(235, 111, 146),
            maroon: Color::Rgb(235, 188, 186),
            peach: Color::Rgb(246, 193, 119),
            yellow: Color::Rgb(246, 193, 119),
            green: Color::Rgb(49, 116, 143),
            teal: Color::Rgb(156, 207, 216),
            sky: Color::Rgb(156, 207, 216),
            sapphire: Color::Rgb(144, 140, 170),
            blue: Color::Rgb(156, 207, 216),
            lavender: Color::Rgb(196, 167, 231),
        };

        Self {
            name: ThemeName::RosePine,
            effects: ThemeEffects::default(),
            semantic: ThemeSemantic {
                text: Color::Rgb(224, 222, 244),
                subtext1: Color::Rgb(144, 140, 170),
                subtext0: Color::Rgb(110, 106, 134),
                overlay0: Color::Rgb(64, 61, 82),
                surface2: Color::Rgb(49, 45, 73),
                surface1: Color::Rgb(65, 60, 90),
                surface0: Color::Rgb(25, 23, 36),
                border: Color::Rgb(92, 88, 120),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sky,
                    categorical.teal,
                    categorical.yellow,
                    categorical.peach,
                    categorical.red,
                    categorical.pink,
                    categorical.mauve,
                    categorical.lavender,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.mauve,
                    medium: categorical.mauve,
                    high: categorical.mauve,
                    empty: Color::Rgb(38, 35, 58),
                },
                stream: ThemeStream {
                    inflow: categorical.blue,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn nightfox() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(219, 225, 245),
            flamingo: Color::Rgb(244, 163, 116),
            pink: Color::Rgb(210, 156, 255),
            mauve: Color::Rgb(187, 154, 247),
            red: Color::Rgb(242, 109, 130),
            maroon: Color::Rgb(219, 118, 126),
            peach: Color::Rgb(244, 163, 116),
            yellow: Color::Rgb(230, 201, 126),
            green: Color::Rgb(126, 207, 143),
            teal: Color::Rgb(86, 205, 205),
            sky: Color::Rgb(131, 206, 255),
            sapphire: Color::Rgb(110, 176, 255),
            blue: Color::Rgb(99, 156, 255),
            lavender: Color::Rgb(175, 152, 252),
        };

        Self {
            name: ThemeName::Nightfox,
            effects: ThemeEffects::default(),
            semantic: ThemeSemantic {
                text: Color::Rgb(210, 220, 248),
                subtext1: Color::Rgb(174, 186, 223),
                subtext0: Color::Rgb(142, 156, 196),
                overlay0: Color::Rgb(92, 104, 145),
                surface2: Color::Rgb(55, 66, 96),
                surface1: Color::Rgb(40, 50, 76),
                surface0: Color::Rgb(26, 33, 54),
                border: Color::Rgb(92, 104, 145),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sky,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.red,
                    categorical.pink,
                    categorical.mauve,
                    categorical.lavender,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.blue,
                    medium: categorical.blue,
                    high: categorical.blue,
                    empty: Color::Rgb(40, 50, 76),
                },
                stream: ThemeStream {
                    inflow: categorical.blue,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn papercolor_light() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(70, 78, 87),
            flamingo: Color::Rgb(181, 90, 60),
            pink: Color::Rgb(161, 85, 170),
            mauve: Color::Rgb(123, 102, 204),
            red: Color::Rgb(200, 72, 65),
            maroon: Color::Rgb(161, 81, 85),
            peach: Color::Rgb(196, 122, 62),
            yellow: Color::Rgb(155, 132, 26),
            green: Color::Rgb(77, 133, 67),
            teal: Color::Rgb(51, 135, 122),
            sky: Color::Rgb(55, 130, 171),
            sapphire: Color::Rgb(67, 112, 182),
            blue: Color::Rgb(52, 98, 175),
            lavender: Color::Rgb(122, 100, 182),
        };

        Self {
            name: ThemeName::PaperColorLight,
            effects: ThemeEffects::default(),
            semantic: ThemeSemantic {
                text: Color::Rgb(55, 62, 72),
                subtext1: Color::Rgb(84, 92, 104),
                subtext0: Color::Rgb(109, 117, 129),
                overlay0: Color::Rgb(150, 145, 133),
                surface2: Color::Rgb(228, 221, 205),
                surface1: Color::Rgb(241, 237, 225),
                surface0: Color::Rgb(250, 248, 240),
                border: Color::Rgb(158, 149, 133),
                white: Color::Black,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sapphire,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.red,
                    categorical.pink,
                    categorical.mauve,
                    categorical.lavender,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.blue,
                    medium: categorical.blue,
                    high: categorical.blue,
                    empty: Color::Rgb(228, 221, 205),
                },
                stream: ThemeStream {
                    inflow: categorical.blue,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn black_hole() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(245, 245, 245),
            flamingo: Color::Rgb(224, 224, 224),
            pink: Color::Rgb(204, 204, 204),
            mauve: Color::Rgb(184, 184, 184),
            red: Color::Rgb(164, 164, 164),
            maroon: Color::Rgb(144, 144, 144),
            peach: Color::Rgb(124, 124, 124),
            yellow: Color::Rgb(104, 104, 104),
            green: Color::Rgb(84, 84, 84),
            teal: Color::Rgb(72, 72, 72),
            sky: Color::Rgb(60, 60, 60),
            sapphire: Color::Rgb(48, 48, 48),
            blue: Color::Rgb(36, 36, 36),
            lavender: Color::Rgb(172, 172, 172),
        };

        Self {
            name: ThemeName::BlackHole,
            effects: ThemeEffects {
                local_enabled: true,
                flicker_hz: 5.2,
                flicker_intensity: 0.18,
                local_burst_duty: 0.16,
                local_burst_hz: 0.75,
                local_idle_intensity: 0.07,
                local_burst_boost: 1.28,
                wave_enabled: true,
                wave_hz: 0.28,
                wave_intensity: 0.16,
                wave_wavelength: 64.0,
                wave_angle_degrees: -34.0,
                wave_mode: WaveMode::Linear,
                particle: ThemeParticleEffect {
                    enabled: true,
                    layer_mode: ParticleLayerMode::Foreground,
                    profile: ParticleProfile::BlackHole,
                    density: 0.05,
                    speed: 1.0,
                    intensity: 0.9,
                },
            },
            semantic: ThemeSemantic {
                text: Color::Rgb(234, 234, 234),
                subtext1: Color::Rgb(188, 188, 188),
                subtext0: Color::Rgb(144, 144, 144),
                overlay0: Color::Rgb(98, 98, 98),
                surface2: Color::Rgb(26, 26, 26),
                surface1: Color::Rgb(12, 12, 12),
                surface0: Color::Rgb(0, 0, 0),
                border: Color::Rgb(116, 116, 116),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sky,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.red,
                    categorical.pink,
                    categorical.mauve,
                    categorical.lavender,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.mauve,
                    medium: categorical.mauve,
                    high: categorical.mauve,
                    empty: Color::Rgb(6, 8, 14),
                },
                stream: ThemeStream {
                    inflow: categorical.lavender,
                    outflow: categorical.teal,
                },
                categorical,
            },
        }
    }

    pub fn obsidian_forge() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(248, 238, 226),
            flamingo: Color::Rgb(255, 171, 130),
            pink: Color::Rgb(230, 165, 145),
            mauve: Color::Rgb(183, 152, 216),
            red: Color::Rgb(255, 108, 92),
            maroon: Color::Rgb(214, 92, 78),
            peach: Color::Rgb(255, 165, 88),
            yellow: Color::Rgb(244, 204, 120),
            green: Color::Rgb(154, 209, 126),
            teal: Color::Rgb(121, 198, 176),
            sky: Color::Rgb(128, 188, 226),
            sapphire: Color::Rgb(103, 163, 214),
            blue: Color::Rgb(84, 136, 196),
            lavender: Color::Rgb(193, 176, 224),
        };

        Self {
            name: ThemeName::ObsidianForge,
            effects: ThemeEffects {
                local_enabled: true,
                flicker_hz: 5.8,
                flicker_intensity: 0.11,
                local_burst_duty: 0.12,
                local_burst_hz: 0.7,
                local_idle_intensity: 0.05,
                local_burst_boost: 1.16,
                wave_enabled: false,
                ..ThemeEffects::default()
            },
            semantic: ThemeSemantic {
                text: Color::Rgb(230, 220, 206),
                subtext1: Color::Rgb(191, 176, 156),
                subtext0: Color::Rgb(156, 137, 114),
                overlay0: Color::Rgb(108, 91, 74),
                surface2: Color::Rgb(74, 62, 52),
                surface1: Color::Rgb(48, 40, 34),
                surface0: Color::Rgb(22, 18, 16),
                border: Color::Rgb(135, 110, 88),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sapphire,
                    categorical.sky,
                    categorical.teal,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.red,
                    categorical.maroon,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.sapphire,
                    medium: categorical.sapphire,
                    high: categorical.peach,
                    empty: Color::Rgb(40, 32, 28),
                },
                stream: ThemeStream {
                    inflow: categorical.sky,
                    outflow: categorical.peach,
                },
                categorical,
            },
        }
    }

    pub fn arctic_whiteout() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(243, 248, 255),
            flamingo: Color::Rgb(222, 236, 252),
            pink: Color::Rgb(206, 228, 252),
            mauve: Color::Rgb(177, 206, 244),
            red: Color::Rgb(226, 112, 126),
            maroon: Color::Rgb(197, 95, 108),
            peach: Color::Rgb(235, 170, 120),
            yellow: Color::Rgb(229, 205, 126),
            green: Color::Rgb(132, 194, 152),
            teal: Color::Rgb(103, 188, 192),
            sky: Color::Rgb(116, 190, 236),
            sapphire: Color::Rgb(94, 167, 219),
            blue: Color::Rgb(84, 142, 206),
            lavender: Color::Rgb(171, 188, 236),
        };

        Self {
            name: ThemeName::ArcticWhiteout,
            effects: ThemeEffects {
                local_enabled: true,
                flicker_hz: 4.6,
                flicker_intensity: 0.08,
                local_burst_duty: 0.16,
                local_burst_hz: 0.6,
                local_idle_intensity: 0.04,
                local_burst_boost: 1.12,
                wave_enabled: true,
                wave_hz: 0.32,
                wave_intensity: 0.08,
                wave_wavelength: 64.0,
                wave_angle_degrees: -35.0,
                wave_mode: WaveMode::Linear,
                particle: ThemeParticleEffect::default(),
            },
            semantic: ThemeSemantic {
                text: Color::Rgb(34, 56, 88),
                subtext1: Color::Rgb(58, 84, 120),
                subtext0: Color::Rgb(87, 111, 146),
                overlay0: Color::Rgb(134, 156, 186),
                surface2: Color::Rgb(214, 226, 242),
                surface1: Color::Rgb(244, 248, 253),
                surface0: Color::Rgb(252, 254, 255),
                border: Color::Rgb(124, 146, 178),
                white: Color::Black,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sky,
                    categorical.sapphire,
                    categorical.teal,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.mauve,
                    categorical.lavender,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.blue,
                    medium: categorical.sapphire,
                    high: categorical.teal,
                    empty: Color::Rgb(222, 232, 245),
                },
                stream: ThemeStream {
                    inflow: categorical.sapphire,
                    outflow: categorical.teal,
                },
                categorical,
            },
        }
    }

    pub fn diamond() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(235, 245, 255),
            flamingo: Color::Rgb(208, 229, 255),
            pink: Color::Rgb(199, 221, 255),
            mauve: Color::Rgb(170, 197, 245),
            red: Color::Rgb(255, 120, 140),
            maroon: Color::Rgb(214, 101, 126),
            peach: Color::Rgb(255, 192, 145),
            yellow: Color::Rgb(245, 220, 140),
            green: Color::Rgb(144, 224, 187),
            teal: Color::Rgb(124, 220, 224),
            sky: Color::Rgb(150, 215, 255),
            sapphire: Color::Rgb(122, 191, 245),
            blue: Color::Rgb(100, 160, 235),
            lavender: Color::Rgb(188, 205, 255),
        };

        Self {
            name: ThemeName::Diamond,
            effects: ThemeEffects {
                local_enabled: true,
                flicker_hz: 7.4,
                flicker_intensity: 0.16,
                local_burst_duty: 0.08,
                local_burst_hz: 0.9,
                local_idle_intensity: 0.02,
                local_burst_boost: 1.35,
                wave_enabled: true,
                wave_hz: 0.44,
                wave_intensity: 0.11,
                wave_wavelength: 52.0,
                wave_angle_degrees: 74.0,
                wave_mode: WaveMode::Linear,
                particle: ThemeParticleEffect {
                    enabled: true,
                    layer_mode: ParticleLayerMode::Background,
                    profile: ParticleProfile::Diamond,
                    density: 0.006,
                    speed: 23.4,
                    intensity: 0.52,
                },
            },
            semantic: ThemeSemantic {
                text: Color::Rgb(228, 240, 255),
                subtext1: Color::Rgb(184, 205, 232),
                subtext0: Color::Rgb(146, 170, 202),
                overlay0: Color::Rgb(93, 114, 145),
                surface2: Color::Rgb(44, 60, 86),
                surface1: Color::Rgb(28, 41, 64),
                surface0: Color::Rgb(12, 20, 36),
                border: Color::Rgb(108, 136, 176),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.red,
                    categorical.peach,
                    categorical.yellow,
                    categorical.green,
                    categorical.teal,
                    categorical.sky,
                    categorical.blue,
                    categorical.mauve,
                ],
                ip_hash: [
                    categorical.rosewater,
                    categorical.sky,
                    categorical.sapphire,
                    categorical.blue,
                    categorical.teal,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.pink,
                    categorical.mauve,
                    categorical.lavender,
                    categorical.flamingo,
                    categorical.red,
                    categorical.maroon,
                ],
                heatmap: ThemeHeatmap {
                    low: categorical.sky,
                    medium: categorical.sky,
                    high: categorical.mauve,
                    empty: Color::Rgb(28, 41, 64),
                },
                stream: ThemeStream {
                    inflow: categorical.sky,
                    outflow: categorical.teal,
                },
                categorical,
            },
        }
    }

    pub fn bioluminescent_reef() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(222, 249, 242),
            flamingo: Color::Rgb(162, 234, 214),
            pink: Color::Rgb(154, 220, 238),
            mauve: Color::Rgb(138, 190, 231),
            red: Color::Rgb(234, 112, 136),
            maroon: Color::Rgb(194, 94, 119),
            peach: Color::Rgb(242, 174, 122),
            yellow: Color::Rgb(238, 214, 128),
            green: Color::Rgb(72, 212, 174),
            teal: Color::Rgb(56, 203, 196),
            sky: Color::Rgb(96, 198, 236),
            sapphire: Color::Rgb(84, 173, 222),
            blue: Color::Rgb(72, 153, 206),
            lavender: Color::Rgb(144, 172, 232),
        };

        Self {
            name: ThemeName::BioluminescentReef,
            effects: ThemeEffects {
                local_enabled: true,
                flicker_hz: 5.6,
                flicker_intensity: 0.10,
                local_burst_duty: 0.14,
                local_burst_hz: 0.8,
                local_idle_intensity: 0.05,
                local_burst_boost: 1.18,
                wave_enabled: true,
                wave_hz: 0.38,
                wave_intensity: 0.14,
                wave_wavelength: 46.0,
                wave_angle_degrees: -55.0,
                wave_mode: WaveMode::Linear,
                particle: ThemeParticleEffect {
                    enabled: true,
                    layer_mode: ParticleLayerMode::Background,
                    profile: ParticleProfile::BioluminescentReef,
                    density: 0.024,
                    speed: 0.42,
                    intensity: 0.72,
                },
            },
            semantic: ThemeSemantic {
                text: Color::Rgb(213, 245, 239),
                subtext1: Color::Rgb(156, 223, 209),
                subtext0: Color::Rgb(116, 193, 181),
                overlay0: Color::Rgb(58, 112, 114),
                surface2: Color::Rgb(26, 67, 74),
                surface1: Color::Rgb(16, 47, 56),
                surface0: Color::Rgb(8, 28, 34),
                border: Color::Rgb(82, 165, 154),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sky,
                    categorical.green,
                    categorical.yellow,
                    categorical.peach,
                    categorical.red,
                    categorical.pink,
                    categorical.mauve,
                    categorical.lavender,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.blue,
                    medium: categorical.teal,
                    high: categorical.green,
                    empty: Color::Rgb(16, 47, 56),
                },
                stream: ThemeStream {
                    inflow: categorical.sky,
                    outflow: categorical.green,
                },
                categorical,
            },
        }
    }

    pub fn sakura() -> Self {
        let categorical = ThemeCategorical {
            rosewater: Color::Rgb(255, 240, 246),
            flamingo: Color::Rgb(255, 188, 220),
            pink: Color::Rgb(255, 142, 198),
            mauve: Color::Rgb(228, 154, 199),
            red: Color::Rgb(214, 102, 138),
            maroon: Color::Rgb(133, 84, 66),
            peach: Color::Rgb(191, 138, 112),
            yellow: Color::Rgb(234, 204, 163),
            green: Color::Rgb(154, 184, 154),
            teal: Color::Rgb(128, 174, 176),
            sky: Color::Rgb(139, 186, 228),
            sapphire: Color::Rgb(111, 162, 206),
            blue: Color::Rgb(93, 138, 188),
            lavender: Color::Rgb(208, 184, 230),
        };

        Self {
            name: ThemeName::Sakura,
            effects: ThemeEffects {
                particle: ThemeParticleEffect {
                    enabled: true,
                    layer_mode: ParticleLayerMode::Background,
                    profile: ParticleProfile::Sakura,
                    density: 0.020,
                    speed: 0.68,
                    intensity: 0.75,
                },
                ..ThemeEffects::default()
            },
            semantic: ThemeSemantic {
                text: Color::Rgb(255, 214, 236),
                subtext1: Color::Rgb(245, 182, 214),
                subtext0: Color::Rgb(222, 149, 178),
                overlay0: Color::Rgb(176, 108, 130),
                surface2: Color::Rgb(121, 76, 82),
                surface1: Color::Rgb(95, 57, 63),
                surface0: Color::Rgb(74, 43, 49),
                border: Color::Rgb(186, 118, 104),
                white: Color::White,
            },
            scale: ThemeScale {
                speed: [
                    categorical.sky,
                    categorical.sapphire,
                    categorical.blue,
                    categorical.teal,
                    categorical.pink,
                    categorical.mauve,
                    categorical.peach,
                    categorical.rosewater,
                ],
                ip_hash: categorical_ip_hash(categorical),
                heatmap: ThemeHeatmap {
                    low: categorical.sky,
                    medium: categorical.pink,
                    high: categorical.maroon,
                    empty: Color::Rgb(73, 55, 67),
                },
                stream: ThemeStream {
                    inflow: categorical.sky,
                    outflow: categorical.teal,
                },
                categorical,
            },
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::catppuccin_mocha()
    }
}

fn categorical_ip_hash(categorical: ThemeCategorical) -> [Color; 14] {
    [
        categorical.rosewater,
        categorical.flamingo,
        categorical.pink,
        categorical.mauve,
        categorical.red,
        categorical.maroon,
        categorical.peach,
        categorical.yellow,
        categorical.green,
        categorical.teal,
        categorical.sky,
        categorical.sapphire,
        categorical.blue,
        categorical.lavender,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_theme_names() -> Vec<ThemeName> {
        vec![
            ThemeName::Andromeda,
            ThemeName::Aurora,
            ThemeName::AyuDark,
            ThemeName::Bubblegum,
            ThemeName::CatppuccinLatte,
            ThemeName::CatppuccinMocha,
            ThemeName::Cyberpunk,
            ThemeName::DeepOcean,
            ThemeName::DeepSky,
            ThemeName::Diamond,
            ThemeName::Gold,
            ThemeName::Dracula,
            ThemeName::EverforestDark,
            ThemeName::GitHubDark,
            ThemeName::GitHubLight,
            ThemeName::GruvboxDark,
            ThemeName::GruvboxLight,
            ThemeName::Inferno,
            ThemeName::Kanagawa,
            ThemeName::MaterialOcean,
            ThemeName::Matrix,
            ThemeName::Monokai,
            ThemeName::Neon,
            ThemeName::Nightfox,
            ThemeName::Nord,
            ThemeName::OneDark,
            ThemeName::ObsidianForge,
            ThemeName::Oxocarbon,
            ThemeName::ArcticWhiteout,
            ThemeName::PaperColorLight,
            ThemeName::BioluminescentReef,
            ThemeName::BlackHole,
            ThemeName::Rainbow,
            ThemeName::RosePine,
            ThemeName::SolarizedDark,
            ThemeName::SolarizedLight,
            ThemeName::Synthwave84,
            ThemeName::TokyoNight,
            ThemeName::Vesper,
            ThemeName::Zenburn,
            ThemeName::Show,
            ThemeName::Sakura,
        ]
    }

    fn relative_luminance(color: Color) -> f64 {
        let (r, g, b) = color_to_rgb(color);
        let to_linear = |v: u8| {
            let v = v as f64 / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        let r = to_linear(r);
        let g = to_linear(g);
        let b = to_linear(b);
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    fn contrast_ratio(a: Color, b: Color) -> f64 {
        let la = relative_luminance(a);
        let lb = relative_luminance(b);
        let (bright, dark) = if la >= lb { (la, lb) } else { (lb, la) };
        (bright + 0.05) / (dark + 0.05)
    }

    fn color_distance(a: Color, b: Color) -> f64 {
        let (ar, ag, ab) = color_to_rgb(a);
        let (br, bg, bb) = color_to_rgb(b);
        let dr = ar as f64 - br as f64;
        let dg = ag as f64 - bg as f64;
        let db = ab as f64 - bb as f64;
        (dr * dr + dg * dg + db * db).sqrt()
    }

    #[test]
    fn test_known_themes_snake_case() {
        let themes = vec![
            ("andromeda", ThemeName::Andromeda),
            ("aurora", ThemeName::Aurora),
            ("ayu_dark", ThemeName::AyuDark),
            ("bubblegum", ThemeName::Bubblegum),
            ("catppuccin_latte", ThemeName::CatppuccinLatte),
            ("catppuccin_mocha", ThemeName::CatppuccinMocha),
            ("cyberpunk", ThemeName::Cyberpunk),
            ("deep_ocean", ThemeName::DeepOcean),
            ("deep_sky", ThemeName::DeepSky),
            ("diamond", ThemeName::Diamond),
            ("gold", ThemeName::Gold),
            ("dracula", ThemeName::Dracula),
            ("everforest_dark", ThemeName::EverforestDark),
            ("github_dark", ThemeName::GitHubDark),
            ("github_light", ThemeName::GitHubLight),
            ("gruvbox_dark", ThemeName::GruvboxDark),
            ("gruvbox_light", ThemeName::GruvboxLight),
            ("inferno", ThemeName::Inferno),
            ("kanagawa", ThemeName::Kanagawa),
            ("material_ocean", ThemeName::MaterialOcean),
            ("matrix", ThemeName::Matrix),
            ("monokai", ThemeName::Monokai),
            ("neon", ThemeName::Neon),
            ("nightfox", ThemeName::Nightfox),
            ("nord", ThemeName::Nord),
            ("one_dark", ThemeName::OneDark),
            ("obsidian_forge", ThemeName::ObsidianForge),
            ("oxocarbon", ThemeName::Oxocarbon),
            ("arctic_whiteout", ThemeName::ArcticWhiteout),
            ("papercolor_light", ThemeName::PaperColorLight),
            ("black_hole", ThemeName::BlackHole),
            ("bioluminescent_reef", ThemeName::BioluminescentReef),
            ("rainbow", ThemeName::Rainbow),
            ("rose_pine", ThemeName::RosePine),
            ("solarized_dark", ThemeName::SolarizedDark),
            ("solarized_light", ThemeName::SolarizedLight),
            ("synthwave_84", ThemeName::Synthwave84),
            ("tokyo_night", ThemeName::TokyoNight),
            ("vesper", ThemeName::Vesper),
            ("zenburn", ThemeName::Zenburn),
            ("show", ThemeName::Show),
            ("sakura", ThemeName::Sakura),
        ];

        for (input, expected) in themes {
            let deserialized: ThemeName = serde_json::from_str(&format!("\"{}\"", input)).unwrap();
            assert_eq!(deserialized, expected, "Failed for input: {}", input);
        }
    }

    #[test]
    fn test_known_themes_display_format() {
        let themes = vec![
            ("Andromeda", ThemeName::Andromeda),
            ("Aurora", ThemeName::Aurora),
            ("Ayu Dark", ThemeName::AyuDark),
            ("Bubblegum", ThemeName::Bubblegum),
            ("Catppuccin Latte", ThemeName::CatppuccinLatte),
            ("Catppuccin Mocha", ThemeName::CatppuccinMocha),
            ("Cyberpunk", ThemeName::Cyberpunk),
            ("Deep Ocean", ThemeName::DeepOcean),
            ("Deep Sky", ThemeName::DeepSky),
            ("Diamond", ThemeName::Diamond),
            ("Gold", ThemeName::Gold),
            ("Dracula", ThemeName::Dracula),
            ("Everforest Dark", ThemeName::EverforestDark),
            ("GitHub Dark", ThemeName::GitHubDark),
            ("GitHub Light", ThemeName::GitHubLight),
            ("Gruvbox Dark", ThemeName::GruvboxDark),
            ("Gruvbox Light", ThemeName::GruvboxLight),
            ("Inferno", ThemeName::Inferno),
            ("Kanagawa", ThemeName::Kanagawa),
            ("Material Ocean", ThemeName::MaterialOcean),
            ("Matrix", ThemeName::Matrix),
            ("Monokai", ThemeName::Monokai),
            ("Neon", ThemeName::Neon),
            ("Nightfox", ThemeName::Nightfox),
            ("Nord", ThemeName::Nord),
            ("One Dark", ThemeName::OneDark),
            ("Obsidian Forge", ThemeName::ObsidianForge),
            ("Oxocarbon", ThemeName::Oxocarbon),
            ("Arctic Whiteout", ThemeName::ArcticWhiteout),
            ("PaperColor Light", ThemeName::PaperColorLight),
            ("Black Hole", ThemeName::BlackHole),
            ("Bioluminescent Reef", ThemeName::BioluminescentReef),
            ("Rainbow", ThemeName::Rainbow),
            ("Rose Pine", ThemeName::RosePine),
            ("Solarized Dark", ThemeName::SolarizedDark),
            ("Solarized Light", ThemeName::SolarizedLight),
            ("Synthwave '84", ThemeName::Synthwave84),
            ("Tokyo Night", ThemeName::TokyoNight),
            ("Vesper", ThemeName::Vesper),
            ("Zenburn", ThemeName::Zenburn),
            ("Show", ThemeName::Show),
            ("Sakura", ThemeName::Sakura),
        ];

        for (input, expected) in themes {
            let deserialized: ThemeName = serde_json::from_str(&format!("\"{}\"", input)).unwrap();
            assert_eq!(deserialized, expected, "Failed for input: {}", input);
        }
    }

    #[test]
    fn test_unknown_themes_default_to_catppuccin_mocha() {
        let unknown_themes = vec![
            "cuppochinmocha",
            "invalid_theme",
            "unknown",
            "",
            "   ",
            "CatpuccinMocha",
            "mocha",
            "dark_theme",
        ];

        for input in unknown_themes {
            let deserialized: ThemeName = serde_json::from_str(&format!("\"{}\"", input)).unwrap();
            assert_eq!(
                deserialized,
                ThemeName::CatppuccinMocha,
                "Unknown theme '{}' should default to CatppuccinMocha",
                input
            );
        }
    }

    #[test]
    fn test_deprecated_theme_aliases_map_to_replacements() {
        let aliases = vec![
            ("catppuccin", ThemeName::CatppuccinMocha),
            ("synthwave84", ThemeName::Synthwave84),
            ("tokyonight", ThemeName::TokyoNight),
            ("flowers", ThemeName::Sakura),
        ];

        for (input, expected) in aliases {
            let deserialized: ThemeName = serde_json::from_str(&format!("\"{}\"", input)).unwrap();
            assert_eq!(
                deserialized, expected,
                "Deprecated alias '{}' mismatch",
                input
            );
        }
    }

    #[test]
    fn test_theme_deserialize_non_string_types_fallback_to_default() {
        let invalid_types = vec!["123", "true", "null", "[]", "{}"];
        for input in invalid_types {
            let deserialized: ThemeName = serde_json::from_str(input).unwrap();
            assert_eq!(
                deserialized,
                ThemeName::CatppuccinMocha,
                "Non-string value '{}' should fallback to default",
                input
            );
        }
    }

    #[test]
    fn test_theme_name_normalization_accepts_case_and_delimiter_variants() {
        let variants = vec![
            ("TOKYO-NIGHT", ThemeName::TokyoNight),
            ("  synthwave '84  ", ThemeName::Synthwave84),
            ("GitHub_Dark", ThemeName::GitHubDark),
        ];

        for (input, expected) in variants {
            let deserialized: ThemeName = serde_json::from_str(&format!("\"{}\"", input)).unwrap();
            assert_eq!(deserialized, expected, "Variant '{}' mismatch", input);
        }
    }

    #[test]
    fn test_theme_default_is_catppuccin_mocha() {
        assert_eq!(ThemeName::default(), ThemeName::CatppuccinMocha);
    }

    #[test]
    fn test_theme_name_roundtrip() {
        for theme in all_theme_names() {
            let serialized = serde_json::to_string(&theme).unwrap();
            let deserialized: ThemeName = serde_json::from_str(&serialized).unwrap();
            assert_eq!(theme, deserialized);
        }
    }

    #[test]
    fn test_solarized_themes_use_canonical_base_tones() {
        let dark = Theme::builtin(ThemeName::SolarizedDark);
        assert_eq!(dark.semantic.surface0, Color::Rgb(0, 43, 54));
        assert_eq!(dark.semantic.surface1, Color::Rgb(7, 54, 66));
        assert_eq!(dark.semantic.surface2, Color::Rgb(88, 110, 117));
        assert_eq!(dark.semantic.border, Color::Rgb(88, 110, 117));
        assert_eq!(dark.semantic.text, Color::Rgb(131, 148, 150));

        let light = Theme::builtin(ThemeName::SolarizedLight);
        assert_eq!(light.semantic.surface0, Color::Rgb(253, 246, 227));
        assert_eq!(light.semantic.surface1, Color::Rgb(238, 232, 213));
        assert_eq!(light.semantic.surface2, Color::Rgb(147, 161, 161));
        assert_eq!(light.semantic.border, Color::Rgb(147, 161, 161));
        assert_eq!(light.semantic.text, Color::Rgb(88, 110, 117));
    }

    #[test]
    fn test_theme_semantic_readability_guards() {
        for name in all_theme_names() {
            let theme = Theme::builtin(name);
            let surface0 = theme.semantic.surface0;

            let text_contrast = contrast_ratio(theme.semantic.text, surface0);
            assert!(
                text_contrast >= 4.5,
                "{name}: text contrast too low ({text_contrast:.2})"
            );

            let subtext_contrast = contrast_ratio(theme.semantic.subtext0, surface0);
            assert!(
                subtext_contrast >= 3.0,
                "{name}: subtext0 contrast too low ({subtext_contrast:.2})"
            );

            let border_contrast = contrast_ratio(theme.semantic.border, surface0);
            assert!(
                border_contrast >= 2.0,
                "{name}: border contrast too low ({border_contrast:.2})"
            );

            let surface_separation = contrast_ratio(theme.semantic.surface2, surface0);
            assert!(
                surface_separation >= 1.2,
                "{name}: surface2 too close to surface0 ({surface_separation:.2})"
            );
        }
    }

    #[test]
    fn test_theme_status_colors_are_distinct() {
        for name in all_theme_names() {
            let theme = Theme::builtin(name);
            let red = theme.scale.categorical.red;
            let yellow = theme.scale.categorical.yellow;
            let green = theme.scale.categorical.green;

            let red_yellow = color_distance(red, yellow);
            let red_green = color_distance(red, green);
            let yellow_green = color_distance(yellow, green);

            assert!(
                red_yellow >= 20.0,
                "{name}: red/yellow too similar ({red_yellow:.1})"
            );
            assert!(
                red_green >= 20.0,
                "{name}: red/green too similar ({red_green:.1})"
            );
            assert!(
                yellow_green >= 20.0,
                "{name}: yellow/green too similar ({yellow_green:.1})"
            );
        }
    }

    #[test]
    fn test_theme_effects_within_comfort_bounds() {
        for name in all_theme_names() {
            let theme = Theme::builtin(name);
            let effects = theme.effects;
            assert!(
                effects.flicker_hz <= 12.0,
                "{name}: flicker_hz too high ({:.2})",
                effects.flicker_hz
            );
            assert!(
                effects.flicker_intensity <= 0.35,
                "{name}: flicker_intensity too high ({:.2})",
                effects.flicker_intensity
            );
            assert!(
                effects.local_burst_boost <= 1.5,
                "{name}: local_burst_boost too high ({:.2})",
                effects.local_burst_boost
            );
            assert!(
                effects.wave_intensity <= 0.2,
                "{name}: wave_intensity too high ({:.2})",
                effects.wave_intensity
            );
        }
    }

    #[test]
    fn test_theme_effects_enabled_flag_tracks_presence_of_effects() {
        let static_theme = Theme::builtin(ThemeName::Nord);
        let effect_theme = Theme::builtin(ThemeName::Diamond);

        assert!(
            !static_theme.effects.enabled(),
            "Nord should report effects disabled"
        );
        assert!(
            effect_theme.effects.enabled(),
            "Diamond should report effects enabled"
        );
    }

    #[test]
    fn test_particle_themes_enable_particle_profiles() {
        let sakura = Theme::builtin(ThemeName::Sakura);
        let matrix = Theme::builtin(ThemeName::Matrix);
        let diamond = Theme::builtin(ThemeName::Diamond);
        let reef = Theme::builtin(ThemeName::BioluminescentReef);
        let black_hole = Theme::builtin(ThemeName::BlackHole);

        assert!(sakura.effects.particle.enabled);
        assert_eq!(sakura.effects.particle.profile, ParticleProfile::Sakura);
        assert_eq!(
            sakura.effects.particle.layer_mode,
            ParticleLayerMode::Background
        );

        assert!(matrix.effects.particle.enabled);
        assert_eq!(matrix.effects.particle.profile, ParticleProfile::Matrix);
        assert_eq!(
            matrix.effects.particle.layer_mode,
            ParticleLayerMode::Background
        );

        assert!(diamond.effects.particle.enabled);
        assert_eq!(diamond.effects.particle.profile, ParticleProfile::Diamond);
        assert_eq!(
            diamond.effects.particle.layer_mode,
            ParticleLayerMode::Background
        );

        assert!(reef.effects.particle.enabled);
        assert_eq!(
            reef.effects.particle.profile,
            ParticleProfile::BioluminescentReef
        );
        assert_eq!(
            reef.effects.particle.layer_mode,
            ParticleLayerMode::Background
        );

        assert!(black_hole.effects.particle.enabled);
        assert_eq!(
            black_hole.effects.particle.profile,
            ParticleProfile::BlackHole
        );
        assert_eq!(
            black_hole.effects.particle.layer_mode,
            ParticleLayerMode::Foreground
        );
    }

    #[test]
    fn test_non_particle_theme_keeps_particle_effects_disabled() {
        let nord = Theme::builtin(ThemeName::Nord);
        assert!(!nord.effects.particle.enabled);
        assert_eq!(nord.effects.particle.layer_mode, ParticleLayerMode::None);
        assert_eq!(nord.effects.particle.profile, ParticleProfile::None);
    }
}
