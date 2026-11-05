// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::time::Duration;

use crate::telemetry::session_stats::SessionStats;
use crate::theme::Theme;
use ratatui::style::Color;

/// Decimal transfer units throughout; style values after padding for alignment.
pub(super) fn render(
    elapsed: Duration,
    uploaded: u64,
    downloaded: u64,
    completed: usize,
    color: bool,
    presentation: (&Theme, &SessionStats),
    lifetime_before_session: (u64, u64),
) -> String {
    let (theme, stats) = presentation;
    let style = |text: String, tint: Color| {
        if color {
            if let Color::Rgb(r, g, b) = tint {
                return format!("\x1b[38;2;{r};{g};{b}m{text}\x1b[0m");
            }
        }
        text
    };
    let palette = theme.scale.categorical;
    let value =
        |text: String, green: bool| style(text, if green { palette.green } else { palette.red });
    let letter_colors = [
        palette.red,
        palette.peach,
        palette.yellow,
        palette.green,
        palette.teal,
        palette.sky,
        palette.sapphire,
        palette.blue,
        palette.mauve,
        palette.pink,
    ];
    let wordmark = "superseedr"
        .chars()
        .zip(letter_colors)
        .map(|(letter, tint)| style(letter.to_string(), tint))
        .collect::<Vec<_>>()
        .join(" ");
    let star = style(
        if color { "\x1b[1mstar" } else { "star" }.to_string(),
        palette.yellow,
    );
    let url = "https://github.com/Jagalite/superseedr";
    let link = if color {
        style(format!("\x1b[4m{url}"), palette.blue)
    } else {
        url.to_string()
    };
    let lifetime_value = |previous: u64, session: u64, green: bool| {
        value(
            format!(
                "{:>15} (+ {})",
                format_transfer(u128::from(previous) + u128::from(session)),
                format_transfer(u128::from(session))
            ),
            green,
        )
    };
    let lifetime_uploaded = lifetime_value(lifetime_before_session.0, uploaded, true);
    let lifetime_downloaded = lifetime_value(lifetime_before_session.1, downloaded, false);
    let ratio = if downloaded == 0 {
        String::new()
    } else {
        format!(
            "  Session ratio {:>15.2}\n",
            uploaded as f64 / downloaded as f64
        )
    };
    let net = i128::from(uploaded) - i128::from(downloaded);
    let uploaded = value(
        format!("{:>15}", format_transfer(u128::from(uploaded))),
        true,
    );
    let downloaded = value(
        format!("{:>15}", format_transfer(u128::from(downloaded))),
        false,
    );
    let net_text = format!(
        "{}{}",
        if net < 0 { "-" } else { "+ " },
        format_transfer(net.unsigned_abs())
    );
    let net = value(format!("{net_text:>15}"), net >= 0);
    let duration = format_duration(elapsed.as_secs());
    let row = |label: &str, text: String| format!("  {label:<14}{text:>15}\n");
    let mut activity = row("Active time", format_duration(stats.active_seconds));
    activity.push('\n');
    for (label, text) in [
        ("Dial attempts", stats.connection_attempts.to_string()),
        ("Unique peers", stats.unique_peers.len().to_string()),
        ("Peak peers", stats.peak_peers.to_string()),
        (
            "Peak upload",
            format!(
                "{}/s",
                format_transfer(u128::from(stats.peak_upload_bps) / 8)
            ),
        ),
        (
            "Peak download",
            format!(
                "{}/s",
                format_transfer(u128::from(stats.peak_download_bps) / 8)
            ),
        ),
    ] {
        activity.push_str(&row(label, text));
    }
    activity.push_str(&ratio);
    activity.push('\n');
    for (label, text) in [
        (
            "Peak seek ↑",
            format_transfer(u128::from(stats.peak_read_seek)),
        ),
        (
            "Peak seek ↓",
            format_transfer(u128::from(stats.peak_write_seek)),
        ),
        ("Peak CPU", format!("{:.2}%", stats.peak_cpu)),
        ("Peak RAM", format_transfer(u128::from(stats.peak_ram))),
    ] {
        activity.push_str(&row(label, text));
    }
    let completed_line = if completed == 0 {
        String::new()
    } else {
        let text = format!(
            "{completed} {}",
            if completed == 1 {
                "torrent"
            } else {
                "torrents"
            }
        );
        format!("  Completed     {text:>15}\n")
    };
    format!(
        "\n\n        {wordmark}\n\n  Duration      {duration:>15}\n{activity}\n  Uploaded      {uploaded}\n  Downloaded    {downloaded}\n  ------------------------------\n  Net           {net}\n\n  Lifetime UL   {lifetime_uploaded}\n  Lifetime DL   {lifetime_downloaded}\n{completed_line}\n  Issues or ideas? Give us a {star}!\n\n  {link}\n\n"
    )
}

fn format_duration(seconds: u64) -> String {
    if seconds >= 3600 {
        format!("{}h {}m", seconds / 3600, seconds / 60 % 60)
    } else if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

fn format_transfer(bytes: u128) -> String {
    const UNITS: [&str; 7] = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];
    let mut amount = bytes as f64;
    let mut unit = 0;
    // Promote values that round to 1000.00 as well, keeping boundary values readable.
    while amount >= 999.995 && unit + 1 < UNITS.len() {
        amount /= 1000.0;
        unit += 1;
    }
    format!("{amount:.2} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(
        elapsed: Duration,
        uploaded: u64,
        downloaded: u64,
        completed: usize,
        color: bool,
    ) -> String {
        super::render(
            elapsed,
            uploaded,
            downloaded,
            completed,
            color,
            (&Theme::default(), &SessionStats::default()),
            (0, 0),
        )
    }

    #[test]
    fn receipt_renders_sampled_session_metrics_in_agreed_order() {
        let mut stats = SessionStats {
            active_seconds: 372,
            connection_attempts: 1284,
            peak_peers: 24,
            peak_upload_bps: 8_000_000,
            peak_download_bps: 16_000_000,
            peak_cpu: 42.1,
            peak_ram: 185_400_000,
            peak_read_seek: 123_000,
            peak_write_seek: 456_000,
            ..Default::default()
        };
        stats.unique_peers.insert("sample-peer".into());
        let text = super::render(
            Duration::from_secs(466),
            15_020_000,
            2_500_000,
            0,
            false,
            (&Theme::default(), &stats),
            (0, 0),
        );
        let labels = [
            "Duration",
            "Active time",
            "Dial attempts",
            "Unique peers",
            "Peak peers",
            "Peak upload",
            "Peak download",
            "Session ratio",
            "Peak seek ↑",
            "Peak seek ↓",
            "Peak CPU",
            "Peak RAM",
            "Uploaded",
            "Downloaded",
            "Net",
            "Lifetime UL",
            "Lifetime DL",
        ];
        let positions: Vec<_> = labels
            .iter()
            .map(|label| text.find(label).expect("metric row"))
            .collect();
        assert!(positions.windows(2).all(|p| p[0] < p[1]));
        assert!(text.contains("6m 12s\n\n"));
        assert!(text.contains("1.00 MB/s"));
        assert!(text.contains("2.00 MB/s"));
        assert!(text.contains("42.10%"));
        assert!(text.contains("185.40 MB"));
        assert!(text.contains("123.00 KB"));
        assert!(!render(Duration::ZERO, 0, 0, 0, false).contains("Session ratio"));
    }

    #[test]
    fn lifetime_totals_include_session_once() {
        let text = super::render(
            Duration::ZERO,
            2_000,
            1_000,
            0,
            false,
            (&Theme::default(), &SessionStats::default()),
            (3_000, 7_000),
        );
        assert!(text.contains("Lifetime UL           5.00 KB (+ 2.00 KB)"));
        assert!(text.contains("Lifetime DL           8.00 KB (+ 1.00 KB)"));
        assert!(!text.contains("SESSION RECEIPT"));
    }

    #[test]
    fn receipt_scales_units_and_short_durations() {
        for (bytes, expected) in [
            (0, "0.00 B"),
            (999, "999.00 B"),
            (1000, "1.00 KB"),
            (1_234_000, "1.23 MB"),
            (1_000_000_000, "1.00 GB"),
            (1_000_000_000_000, "1.00 TB"),
            (1_000_000_000_000_000, "1.00 PB"),
            (999_999, "1.00 MB"),
        ] {
            assert_eq!(format_transfer(bytes), expected);
        }
        for (seconds, expected) in [
            (0, "0s"),
            (45, "45s"),
            (60, "1m 0s"),
            (125, "2m 5s"),
            (3600, "1h 0m"),
        ] {
            let text = render(Duration::from_secs(seconds), 1_234_000, 234_000, 0, false);
            assert!(text.contains(&format!("Duration      {expected:>15}")));
            assert!(text.contains("+ 1.00 MB"));
            assert!(!text.contains("Completed"));
        }
        let text = render(Duration::ZERO, 0, 1250, 0, false);
        assert!(text.contains("-1.25 KB"));
    }

    #[test]
    fn receipt_uses_decimal_gb_and_agreed_layout() {
        let text = render(
            Duration::from_secs(8040),
            6_200_000_000,
            1_800_000_000,
            3,
            false,
        );
        assert!(text.contains("Duration               2h 14m"));
        assert!(text.contains("Uploaded              6.20 GB\n  Downloaded            1.80 GB"));
        assert!(text.contains("------------------------------\n  Net                 + 4.40 GB"));
        assert!(text.contains("3 torrents"));
        assert!(text.contains(
            "Issues or ideas? Give us a star!\n\n  https://github.com/Jagalite/superseedr"
        ));
        assert!(!text.contains('\x1b'));
        assert!(text.contains("s u p e r s e e d r"));
        assert!(!text.contains("superseedr/"));
    }

    #[test]
    fn receipt_colors_values_and_handles_negative_zero_and_large_totals() {
        let text = render(Duration::ZERO, 0, 1_000_000_000, 1, true);
        assert!(text.contains("\x1b[38;2;166;227;161m         0.00 B\x1b[0m"));
        assert!(text.contains("\x1b[38;2;243;139;168m        1.00 GB\x1b[0m"));
        assert!(text.contains("\x1b[38;2;243;139;168m       -1.00 GB\x1b[0m"));
        assert!(text.contains("1 torrent\n"));
        assert!(text.contains(
            "\x1b[38;2;137;180;250m\x1b[4mhttps://github.com/Jagalite/superseedr\x1b[0m"
        ));
        assert!(text.contains("Give us a \x1b[38;2;249;226;175m\x1b[1mstar\x1b[0m!"));
        let palette = Theme::default().scale.categorical;
        let colors = [
            palette.red,
            palette.peach,
            palette.yellow,
            palette.green,
            palette.teal,
            palette.sky,
            palette.sapphire,
            palette.blue,
            palette.mauve,
            palette.pink,
        ];
        let distinct: std::collections::HashSet<_> = colors.into_iter().collect();
        assert_eq!(distinct.len(), 10);
        for (letter, tint) in "superseedr".chars().zip(colors) {
            let Color::Rgb(r, g, b) = tint else {
                panic!("expected RGB palette")
            };
            assert!(text.contains(&format!("\x1b[38;2;{r};{g};{b}m{letter}\x1b[0m")));
        }
        assert!(render(Duration::ZERO, 0, 0, 0, false).contains("+ 0.00 B"));
        assert!(render(Duration::ZERO, u64::MAX, 0, 0, false).contains("+ 18.45 EB"));
    }
}
