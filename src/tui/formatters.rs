// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::theme::ThemeContext;
use ratatui::style::{Color, Style};
use std::path::Path;
use std::time::Duration;

use ratatui::prelude::Constraint;
use ratatui::prelude::Direction;
use ratatui::prelude::Layout;
use ratatui::prelude::Rect;
use ratatui::text::{Line, Span};

use crate::app::GraphDisplayMode;

pub fn format_speed(bits_per_second: u64) -> String {
    if bits_per_second < 1_000 {
        format!("{} bps", bits_per_second)
    } else if bits_per_second < 1_000_000 {
        format!("{:.1} Kbps", bits_per_second as f64 / 1_000.0)
    } else if bits_per_second < 1_000_000_000 {
        format!("{:.2} Mbps", bits_per_second as f64 / 1_000_000.0)
    } else if bits_per_second < 1_000_000_000_000 {
        format!("{:.2} Gbps", bits_per_second as f64 / 1_000_000_000.0)
    } else if bits_per_second < 1_000_000_000_000_000 {
        format!("{:.2} Tbps", bits_per_second as f64 / 1_000_000_000_000.0)
    } else if bits_per_second < 1_000_000_000_000_000_000 {
        format!(
            "{:.2} Pbps",
            bits_per_second as f64 / 1_000_000_000_000_000.0
        )
    } else {
        format!(
            "{:.2} Ebps",
            bits_per_second as f64 / 1_000_000_000_000_000_000.0
        )
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    const TB: u64 = 1024 * GB;

    if bytes < KB {
        format!("{} B", bytes)
    } else if bytes < MB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else if bytes < GB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes < TB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else {
        format!("{:.2} TB", bytes as f64 / TB as f64)
    }
}

pub fn format_memory(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    if bytes < KB {
        format!("{} B", bytes)
    } else if bytes < MB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else if bytes < GB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    }
}

pub fn format_time(seconds: u64) -> String {
    let mut s = seconds;
    let days = s / (24 * 3600);
    s %= 24 * 3600;
    let hours = s / 3600;
    s %= 3600;
    let minutes = s / 60;
    let remaining_seconds = s % 60;

    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{}d", days));
    }
    if hours > 0 {
        parts.push(format!("{}h", hours));
    }
    if minutes > 0 {
        parts.push(format!("{}m", minutes));
    }
    if remaining_seconds > 0 || parts.is_empty() {
        parts.push(format!("{}s", remaining_seconds));
    }

    parts.join(" ")
}

pub fn format_duration(duration: Duration) -> String {
    if duration == Duration::MAX {
        return "∞".to_string();
    }
    if duration.as_secs() == 0 {
        return "Done".to_string();
    }

    let mut secs = duration.as_secs();

    let days = secs / (24 * 3600);
    secs %= 24 * 3600;
    let hours = secs / 3600;
    secs %= 3600;
    let minutes = secs / 60;
    let seconds = secs % 60;

    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{}d", days));
    }
    if hours > 0 {
        parts.push(format!("{}h", hours));
    }
    if minutes > 0 && days == 0 {
        // Only show minutes if not showing days
        parts.push(format!("{}m", minutes));
    }
    if seconds > 0 && days == 0 && hours == 0 {
        // Only show seconds if very short
        parts.push(format!("{}s", seconds));
    }

    if parts.is_empty() {
        "Done".to_string()
    } else {
        parts.join(" ")
    }
}

pub fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

pub fn centered_fixed_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

pub fn path_to_string(path: Option<&Path>) -> String {
    path.map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "Not Set".to_string())
}

pub fn ip_to_color(ctx: &ThemeContext, ip: &str) -> Color {
    let colors = ctx.theme.scale.ip_hash;

    let hash = ip
        .as_bytes()
        .iter()
        .fold(0u32, |acc, &b| acc.wrapping_add(b as u32));

    colors[hash as usize % colors.len()]
}

pub fn speed_to_style(ctx: &ThemeContext, speed_bps: u64) -> Style {
    if speed_bps == 0 {
        Style::default() // Let the main row style handle the color for zero speed
    } else if speed_bps < 50_000 {
        ctx.apply(Style::default().fg(ctx.theme.scale.speed[0]))
    } else if speed_bps < 500_000 {
        ctx.apply(Style::default().fg(ctx.theme.scale.speed[1]))
    } else if speed_bps < 2_000_000 {
        ctx.apply(Style::default().fg(ctx.theme.scale.speed[2]))
    } else if speed_bps < 10_000_000 {
        ctx.apply(Style::default().fg(ctx.theme.scale.speed[3]))
    } else if speed_bps < 20_000_000 {
        ctx.apply(Style::default().fg(ctx.theme.scale.speed[4]))
    } else if speed_bps < 50_000_000 {
        // < 50 Mbps
        ctx.apply(Style::default().fg(ctx.theme.scale.speed[5]))
    } else if speed_bps < 100_000_000 {
        // < 100 Mbps
        ctx.apply(Style::default().fg(ctx.theme.scale.speed[6]))
    } else {
        // >= 100 Mbps
        ctx.apply(Style::default().fg(ctx.theme.scale.speed[7]))
    }
}

pub fn terminal_text_width(input: &str) -> usize {
    Line::from(input).width()
}

pub fn truncate_with_ellipsis(input: &str, max_width: usize) -> String {
    if terminal_text_width(input) <= max_width {
        return input.to_string();
    }
    if max_width <= 3 {
        return ".".repeat(max_width);
    }

    let prefix = terminal_width_prefix(input, max_width - 3);
    format!("{prefix}...")
}

pub fn truncate_middle_with_ellipsis(input: &str, max_width: usize) -> String {
    if terminal_text_width(input) <= max_width {
        return input.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }

    let remaining_width = max_width - 1;
    let head_width = remaining_width.div_ceil(2);
    let tail_width = remaining_width / 2;
    let head = terminal_width_prefix(input, head_width);
    let tail = terminal_width_suffix(input, tail_width);
    format!("{head}…{tail}")
}

fn terminal_width_prefix(input: &str, max_width: usize) -> &str {
    let mut end = 0;
    for (index, character) in input.char_indices() {
        let candidate_end = index + character.len_utf8();
        if terminal_text_width(&input[..candidate_end]) > max_width {
            break;
        }
        end = candidate_end;
    }
    &input[..end]
}

fn terminal_width_suffix(input: &str, max_width: usize) -> &str {
    let mut start = input.len();
    for (index, _) in input.char_indices().rev() {
        if terminal_text_width(&input[index..]) > max_width {
            break;
        }
        start = index;
    }

    // Avoid starting a suffix with a combining character whose base did not fit.
    while start < input.len() {
        let character = input[start..]
            .chars()
            .next()
            .expect("start is within the string");
        let character_end = start + character.len_utf8();
        if terminal_text_width(&input[start..character_end]) > 0 {
            break;
        }
        start = character_end;
    }

    &input[start..]
}

pub(crate) fn anonymize_preserving_shape(input: &str) -> String {
    let seed = stable_string_seed(input);
    let mut anonymized = String::with_capacity(input.len());
    let mut last_was_separator = false;

    for (idx, ch) in input.chars().enumerate() {
        if ch == '/' || ch == '\\' {
            if anonymized.ends_with(' ') {
                anonymized.pop();
            }
            anonymized.push(ch);
            last_was_separator = false;
        } else if ch.is_alphabetic() {
            anonymized.push(anonymized_shape_letter(seed, idx));
            last_was_separator = false;
        } else if !anonymized.is_empty()
            && !last_was_separator
            && !anonymized.ends_with('/')
            && !anonymized.ends_with('\\')
        {
            anonymized.push(' ');
            last_was_separator = true;
        }
    }

    if anonymized.ends_with(' ') {
        anonymized.pop();
    }
    anonymized
}

fn stable_string_seed(input: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in input.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn anonymized_shape_letter(seed: u64, idx: usize) -> char {
    let mut state = seed ^ ((idx as u64 + 1).wrapping_mul(0x9e3779b97f4a7c15));
    state ^= state >> 30;
    state = state.wrapping_mul(0xbf58476d1ce4e5b9);
    state ^= state >> 27;
    state = state.wrapping_mul(0x94d049bb133111eb);
    state ^= state >> 31;

    (b'a' + (state % 26) as u8) as char
}

pub fn calculate_nice_upper_bound(speed_bps: u64) -> u64 {
    if speed_bps == 0 {
        return 10_000;
    }

    let exponent = (speed_bps as f64).log10().floor();
    let power_of_10 = 10.0_f64.powf(exponent);

    // Normalize the speed to be between 1 and 10
    let normalized_speed = (speed_bps as f64) / power_of_10;

    // Find the next "nice" number that is greater than the normalized speed.
    // This creates a more granular and tighter upper bound for the graph.
    let nice_multiplier = if normalized_speed < 1.0 {
        1.0
    } else if normalized_speed < 1.5 {
        1.5
    } else if normalized_speed < 2.0 {
        2.0
    } else if normalized_speed < 2.5 {
        2.5
    } else if normalized_speed < 3.0 {
        3.0
    } else if normalized_speed < 4.0 {
        4.0
    } else if normalized_speed < 5.0 {
        5.0
    } else if normalized_speed < 6.0 {
        6.0
    } else if normalized_speed < 7.0 {
        7.0
    } else if normalized_speed < 8.0 {
        8.0
    } else if normalized_speed < 9.0 {
        9.0
    } else {
        10.0
    };

    (nice_multiplier * power_of_10) as u64
}

pub fn format_countdown(duration: Duration) -> String {
    if duration == Duration::MAX {
        return "N/A".to_string();
    }
    if duration.as_secs() == 0 {
        return "Now".to_string();
    }

    let secs = duration.as_secs();

    let minutes = secs / 60;
    let seconds = secs % 60;

    let mut parts = Vec::new();
    if minutes > 0 {
        parts.push(format!("{}m", minutes));
    }
    if seconds > 0 || parts.is_empty() {
        parts.push(format!("{}s", seconds));
    }

    parts.join(" ").to_string()
}

pub fn format_limit_bps(bps: u64) -> String {
    if crate::config::is_unlimited_rate_limit_bps(bps) {
        "Unlimited".to_string()
    } else {
        format_speed(bps)
    }
}

pub fn auto_download_limit_applied(
    configured_download_limit_bps: u64,
    effective_download_limit_bps: u64,
) -> bool {
    effective_download_limit_bps > 0
        && effective_download_limit_bps != configured_download_limit_bps
}

pub fn format_graph_time_label(duration_secs: usize) -> String {
    const MINUTE: usize = 60;
    const HOUR: usize = 60 * MINUTE;
    const DAY: usize = 24 * HOUR;

    if duration_secs < MINUTE {
        format!("-{}s", duration_secs)
    } else if duration_secs < HOUR {
        format!("-{}m", duration_secs / MINUTE)
    } else if duration_secs < DAY {
        format!("-{}h", duration_secs / HOUR)
    } else {
        format!("-{}d", duration_secs / DAY)
    }
}

pub fn generate_x_axis_labels(
    ctx: &ThemeContext,
    graph_mode: GraphDisplayMode,
) -> Vec<Span<'static>> {
    let labels_str: Vec<String> = match graph_mode {
        GraphDisplayMode::OneMinute => (0..=4)
            .map(|i| format_graph_time_label(60 - i * 15))
            .collect(),
        GraphDisplayMode::FiveMinutes => (0..=5)
            .map(|i| format_graph_time_label(300 - i * 60))
            .collect(),
        GraphDisplayMode::TenMinutes => (0..=5)
            .map(|i| format_graph_time_label(600 - i * 120))
            .collect(),
        GraphDisplayMode::ThirtyMinutes => (0..=6)
            .map(|i| format_graph_time_label(1800 - i * 300))
            .collect(),
        GraphDisplayMode::OneHour => (0..=6)
            .map(|i| format_graph_time_label(3600 - i * 600)) // Every 10 minutes
            .collect(),
        GraphDisplayMode::ThreeHours => (0..=6)
            .map(|i| format_graph_time_label(3 * 3600 - i * 1800)) // 10800 - i * 1800
            .collect(),
        GraphDisplayMode::TwelveHours => (0..=4) // Changed from 0..=5 to 0..=4
            .map(|i| format_graph_time_label(12 * 3600 - i * 3 * 3600)) // 43200 - i * 10800
            .collect(),
        GraphDisplayMode::TwentyFourHours => (0..=6)
            .map(|i| format_graph_time_label(86400 - i * 14400)) // Every 4 hours
            .collect(),
        GraphDisplayMode::SevenDays => (0..=7)
            .map(|i| format_graph_time_label(7 * 86_400 - i * 86_400)) // Daily ticks
            .collect(),
        GraphDisplayMode::ThirtyDays => (0..=6)
            .map(|i| format_graph_time_label(30 * 86_400 - i * 5 * 86_400)) // Every 5 days
            .collect(),
        GraphDisplayMode::OneYear => (0..=12)
            .map(|i| format_graph_time_label(365 * 86_400 - i * 30 * 86_400)) // ~monthly
            .collect(),
    };

    // Convert the strings to styled Spans, replacing the last label with "Now".
    let mut x_labels: Vec<Span> = labels_str
        .into_iter()
        .map(|s| {
            Span::styled(
                s,
                ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
            )
        })
        .collect();
    if let Some(last) = x_labels.last_mut() {
        *last = Span::styled(
            "Now",
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
        );
    }
    x_labels
}

pub fn parse_peer_id(peer_id: &[u8]) -> String {
    crate::peer_manager::parse_peer_client(peer_id)
}

pub fn format_latency(duration: Duration) -> String {
    let micros = duration.as_micros();
    if micros < 1000 {
        format!("{} µs", micros)
    } else if micros < 1_000_000 {
        format!("{:.2} ms", micros as f64 / 1000.0)
    } else {
        format!("{:.2} s", micros as f64 / 1_000_000.0)
    }
}

pub fn format_iops(iops: u32) -> String {
    format!("{} ops/s", iops)
}

pub fn sanitize_text(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_truncation_respects_terminal_width_and_small_limits() {
        assert_eq!(truncate_with_ellipsis("Wide 界界 name", 9), "Wide ...");
        assert_eq!(truncate_with_ellipsis("abcdef", 2), "..");
        assert_eq!(truncate_with_ellipsis("abcdef", 0), "");
    }

    #[test]
    fn middle_truncation_preserves_a_distinct_suffix_with_wide_text() {
        let input = format!("{}alpha.bin", "界".repeat(12));
        let truncated = truncate_middle_with_ellipsis(&input, 16);

        assert!(terminal_text_width(&truncated) <= 16);
        assert!(truncated.contains('…'));
        assert!(truncated.ends_with("pha.bin"));
    }

    #[test]
    fn auto_download_limit_applied_detects_effective_auto_cap() {
        assert!(auto_download_limit_applied(0, 500_000_000));
        assert!(auto_download_limit_applied(800_000_000, 500_000_000));
    }

    #[test]
    fn auto_download_limit_applied_keeps_unlimited_and_configured_caps_normal() {
        assert!(!auto_download_limit_applied(0, 0));
        assert!(!auto_download_limit_applied(500_000_000, 500_000_000));
    }

    #[test]
    fn format_speed_scales_beyond_gigabits() {
        assert_eq!(format_speed(1_000_000_000_000), "1.00 Tbps");
        assert_eq!(format_speed(1_000_000_000_000_000), "1.00 Pbps");
        assert_eq!(format_speed(1_000_000_000_000_000_000), "1.00 Ebps");
    }
}
