// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::theme; // You'll need this for ip_to_color and speed_to_style
use ratatui::style::{Color, Style}; // And these
use std::path::Path;
use std::time::Duration;

use ratatui::prelude::Constraint;
use ratatui::prelude::Direction;
use ratatui::prelude::Layout;
use ratatui::prelude::Rect;
use ratatui::text::Span;

use crate::app::GraphDisplayMode;

pub fn format_speed(bits_per_second: u64) -> String {
    if bits_per_second < 1_000 {
        format!("{} bps", bits_per_second)
    } else if bits_per_second < 1_000_000 {
        format!("{:.1} Kbps", bits_per_second as f64 / 1_000.0)
    } else if bits_per_second < 1_000_000_000 {
        format!("{:.2} Mbps", bits_per_second as f64 / 1_000_000.0)
    } else {
        format!("{:.2} Gbps", bits_per_second as f64 / 1_000_000_000.0)
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

pub fn path_to_string(path: Option<&Path>) -> String {
    path.map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "Not Set".to_string())
}

pub fn ip_to_color(ip: &str) -> Color {
    // A curated list of pastel-like colors from your theme.
    let colors = [
        theme::ROSEWATER,
        theme::FLAMINGO,
        theme::PINK,
        theme::MAUVE,
        theme::RED,
        theme::MAROON,
        theme::PEACH,
        theme::YELLOW,
        theme::GREEN,
        theme::TEAL,
        theme::SKY,
        theme::SAPPHIRE,
        theme::BLUE,
        theme::LAVENDER,
    ];

    // A simple, fast hashing function: sum the byte values of the IP string.
    // Using wrapping_add prevents overflow.
    let hash = ip
        .as_bytes()
        .iter()
        .fold(0u32, |acc, &b| acc.wrapping_add(b as u32));

    // Use the hash to pick a color from the palette.
    colors[hash as usize % colors.len()]
}

pub fn speed_to_style(speed_bps: u64) -> Style {
    if speed_bps == 0 {
        Style::default() // Let the main row style handle the color for zero speed
    } else if speed_bps < 50_000 {
        // < 50 KB/s
        Style::default().fg(theme::SKY)
    } else if speed_bps < 500_000 {
        // < 500 KB/s
        Style::default().fg(theme::GREEN)
    } else if speed_bps < 2_000_000 {
        // < 2 MB/s
        Style::default().fg(theme::YELLOW)
    } else if speed_bps < 10_000_000 {
        // < 10 Mbps
        Style::default().fg(theme::PEACH)
    } else if speed_bps < 20_000_000 {
        // < 20 Mbps
        Style::default().fg(theme::MAROON)
    } else if speed_bps < 50_000_000 {
        // < 50 Mbps
        Style::default().fg(theme::RED)
    } else if speed_bps < 100_000_000 {
        // < 100 Mbps
        Style::default().fg(theme::FLAMINGO)
    } else {
        // >= 100 Mbps
        Style::default().fg(theme::PINK)
    }
}

pub fn truncate_with_ellipsis(s: &str, max_len: usize) -> String {
    if s.chars().count() > max_len {
        // Take `max_len - 3` characters to make room for "..."
        let truncated: String = s.chars().take(max_len.saturating_sub(3)).collect();
        format!("{}...", truncated)
    } else {
        s.to_string()
    }
}

pub fn calculate_nice_upper_bound(speed_bps: u64) -> u64 {
    if speed_bps == 0 {
        // Default to 10 Kbps if speed is 0.
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
    if bps == 0 {
        "Unlimited".to_string()
    } else {
        format_speed(bps)
    }
}

pub fn format_graph_time_label(duration_secs: usize) -> String {
    const MINUTE: usize = 60;
    const HOUR: usize = 60 * MINUTE;

    // --- THIS IS THE FIX ---
    // Add a new branch to handle durations less than a minute.
    if duration_secs < MINUTE {
        format!("-{}s", duration_secs)
    } else if duration_secs < HOUR {
        format!("-{}m", duration_secs / MINUTE)
    } else {
        format!("-{}h", duration_secs / HOUR)
    }
}

pub fn generate_x_axis_labels(graph_mode: GraphDisplayMode) -> Vec<Span<'static>> {
    // Generate a specific set of "nice" label strings for each time window.
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
    };

    // Convert the strings to styled Spans, replacing the last label with "Now".
    let mut x_labels: Vec<Span> = labels_str
        .into_iter()
        .map(|s| Span::styled(s, Style::default().fg(theme::SUBTEXT0)))
        .collect();
    if let Some(last) = x_labels.last_mut() {
        *last = Span::styled("Now", Style::default().fg(theme::SUBTEXT0));
    }
    x_labels
}

pub fn parse_peer_id(peer_id: &[u8]) -> String {
    if peer_id.len() < 8 {
        return "Unknown".to_string();
    }

    // Standard convention: -XXYYYY- where XX is client code and YYYY is version
    if peer_id[0] == b'-' && peer_id[7] == b'-' {
        let client_code = &peer_id[1..3];
        let version = &peer_id[3..7];

        let client_name = match client_code {
            b"TR" => "Transmission",
            b"UT" => "µTorrent",
            b"qB" => "qBittorrent",
            b"AZ" => "Vuze/Azureus",
            b"LT" => "libtorrent",
            b"DE" => "Deluge",
            b"S" | b"SD" => "Shadow",
            _ => {
                return format!(
                    "Unknown ({}{})",
                    String::from_utf8_lossy(client_code),
                    String::from_utf8_lossy(version)
                )
            }
        };

        return format!("{} {}", client_name, String::from_utf8_lossy(version));
    }

    // Some clients use a different format
    if peer_id.starts_with(b"M")
        && peer_id[1..8]
            .iter()
            .all(|c| c.is_ascii_digit() || *c == b'-')
    {
        return "BitComet".to_string();
    }

    "Unknown".to_string()
}

pub fn format_permits_spans(
    label: &str,
    used: usize,
    total: usize,
    base_color: Color,
) -> Vec<Span<'_>> {
    let usage_ratio = if total > 0 {
        used as f64 / total as f64
    } else {
        0.0
    };

    let status_color = if usage_ratio > 0.9 {
        theme::RED
    } else if usage_ratio > 0.7 {
        theme::YELLOW
    } else {
        theme::TEXT
    };

    // CHANGE: We now return the Vec<Span> directly.
    vec![
        Span::styled(label, Style::default().fg(base_color)),
        Span::styled(
            format!(" {} / {}", used, total),
            Style::default().fg(status_color),
        ),
    ]
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

pub fn format_limit_delta(current: usize, last: usize) -> Span<'static> {
    let delta = current as isize - last as isize;
    if delta == 0 {
        return Span::raw("");
    }
    let (sign, style) = if delta > 0 {
        ("+", Style::default().fg(theme::GREEN))
    } else {
        ("-", Style::default().fg(theme::RED))
    };
    Span::styled(format!(" ({}{})", sign, delta.abs()), style)
}
