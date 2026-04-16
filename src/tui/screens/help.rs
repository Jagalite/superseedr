// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::{AppMode, AppState};
use crate::config::{
    is_shared_config_mode, local_settings_path, resolve_host_watch_path, runtime_log_dir,
    shared_inbox_path, shared_settings_path, Settings,
};
use crate::theme::ThemeContext;
use crate::tui::formatters::{centered_rect, truncate_with_ellipsis};
use crate::tui::screen_context::ScreenContext;
use crate::tui::screens::journal::journal_help_rows;
use crate::tui::view::calculate_player_stats;
use ratatui::crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEventKind};
use ratatui::{prelude::*, widgets::*};

fn display_path_or_disabled(path: Option<std::path::PathBuf>) -> String {
    path.map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|| "Disabled".to_string())
}

fn build_help_footer_entries(
    settings: &Settings,
    app_state: &AppState,
) -> Vec<(&'static str, String)> {
    let log_path_str = runtime_log_dir()
        .map(|path| path.join("app*.log"))
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|| "Unknown location".to_string());

    let mut entries = if is_shared_config_mode() {
        vec![
            (
                "Settings",
                shared_settings_path()
                    .map(|path| path.to_string_lossy().to_string())
                    .unwrap_or_else(|| "Unknown location".to_string()),
            ),
            ("Log Files", log_path_str),
            (
                "Host Watch",
                display_path_or_disabled(resolve_host_watch_path(settings)),
            ),
            (
                "Shared Inbox",
                shared_inbox_path()
                    .map(|path| path.to_string_lossy().to_string())
                    .unwrap_or_else(|| "Unknown location".to_string()),
            ),
        ]
    } else {
        let settings_path_str = local_settings_path()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_else(|| "Unknown location".to_string());
        let watch_path_str = crate::config::get_watch_path()
            .map(|(system_watch, _)| system_watch.to_string_lossy().to_string())
            .unwrap_or_else(|| "Disabled".to_string());
        vec![
            ("Settings", settings_path_str),
            ("Log Files", log_path_str),
            ("Watch Dir", watch_path_str),
        ]
    };

    if let Some(cluster_role) = app_state.cluster_role_label.as_ref() {
        entries.push(("Cluster", cluster_role.clone()));
    }
    if let Some(runtime_label) = app_state.cluster_runtime_label.as_ref() {
        entries.push(("Runtime", runtime_label.clone()));
    }

    entries
}

fn draw_help_footer(
    f: &mut Frame,
    area: Rect,
    entries: &[(&'static str, String)],
    ctx: &ThemeContext,
) {
    let footer_block =
        Block::default().border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.border)));
    let footer_inner_area = footer_block.inner(area);
    f.render_widget(footer_block, area);
    let footer_lines = entries
        .iter()
        .map(|(label, value)| {
            let reserved = label.len().saturating_add(2);
            let available_width = (footer_inner_area.width as usize).saturating_sub(reserved);
            Line::from(vec![
                Span::styled(
                    format!("{}: ", label),
                    ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
                ),
                Span::styled(
                    truncate_with_ellipsis(value, available_width),
                    ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
                ),
            ])
        })
        .collect::<Vec<_>>();
    let footer_paragraph =
        Paragraph::new(footer_lines).style(ctx.apply(Style::default().fg(ctx.theme.semantic.text)));
    f.render_widget(footer_paragraph, footer_inner_area);
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HelpAction {
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HelpEffect {
    ToNormal,
}

#[derive(Default)]
pub struct HelpReduceResult {
    pub consumed: bool,
    pub effects: Vec<HelpEffect>,
}

fn map_key_to_help_action(key_code: KeyCode, key_kind: KeyEventKind) -> Option<HelpAction> {
    if key_kind == KeyEventKind::Press
        && (key_code == KeyCode::Esc || key_code == KeyCode::Char('m'))
    {
        return Some(HelpAction::Close);
    }
    None
}

pub fn reduce_help_action(action: HelpAction) -> HelpReduceResult {
    match action {
        HelpAction::Close => HelpReduceResult {
            consumed: true,
            effects: vec![HelpEffect::ToNormal],
        },
    }
}

pub fn execute_help_effects(app_state: &mut AppState, effects: Vec<HelpEffect>) {
    for effect in effects {
        match effect {
            HelpEffect::ToNormal => app_state.mode = AppMode::Normal,
        }
    }
}

pub fn handle_event(event: CrosstermEvent, app_state: &mut AppState) {
    if !matches!(app_state.mode, AppMode::Help) {
        return;
    }

    if let CrosstermEvent::Key(key) = event {
        if let Some(action) = map_key_to_help_action(key.code, key.kind) {
            let reduced = reduce_help_action(action);
            if reduced.consumed {
                execute_help_effects(app_state, reduced.effects);
            }
        }
    }
}

pub fn draw(f: &mut Frame, screen: &ScreenContext<'_>) {
    let app_state = screen.ui;
    let settings = screen.settings;
    let ctx = screen.theme;
    let footer_entries = build_help_footer_entries(settings, app_state);
    let footer_height = footer_entries.len() as u16;

    let area = centered_rect(60, 100, f.area());
    f.render_widget(Clear, area);

    if let Some(warning_text) = &app_state.system_warning {
        let warning_width = area.width.saturating_sub(2).max(1) as usize;
        let warning_lines = (warning_text.len() as f64 / warning_width as f64).ceil() as u16;
        let warning_block_height = warning_lines.saturating_add(2).max(3);
        let max_warning_height = (area.height as f64 * 0.25).round() as u16;
        let final_warning_height = warning_block_height.min(max_warning_height);
        let chunks = Layout::vertical([
            Constraint::Length(final_warning_height),
            Constraint::Min(0),
            Constraint::Length(footer_height),
        ])
        .split(area);

        let warning_paragraph = Paragraph::new(warning_text.as_str())
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(ctx.apply(Style::default().fg(ctx.state_error()))),
            )
            .style(ctx.apply(Style::default().fg(ctx.state_warning())));
        f.render_widget(warning_paragraph, chunks[0]);
        draw_help_table(f, app_state, chunks[1], ctx);
        draw_help_footer(f, chunks[2], &footer_entries, ctx);
    } else {
        let chunks =
            Layout::vertical([Constraint::Min(0), Constraint::Length(footer_height)]).split(area);
        draw_help_table(f, app_state, chunks[0], ctx);
        draw_help_footer(f, chunks[1], &footer_entries, ctx);
    }
}

fn draw_help_table(f: &mut Frame, app_state: &AppState, area: Rect, ctx: &ThemeContext) {
    let mode = &app_state.mode;

    let (lvl, progress) = calculate_player_stats(app_state);

    // Bar styling
    let gauge_width = 15;
    let filled_len = (progress * gauge_width as f64).round() as usize;
    let empty_len = gauge_width - filled_len;
    let gauge_str = format!("[{}{}]", "=".repeat(filled_len), "-".repeat(empty_len));

    // Text styling
    let level_text = format!("Level {} ({:.0}%)", lvl, progress * 100.0);

    let (title, mut rows) = match mode {
        AppMode::Normal | AppMode::Welcome | AppMode::Help => (
            " Manual / Help ",
            vec![
                Row::new(vec![Cell::from(Span::styled(
                    "General Controls",
                    ctx.apply(Style::default().fg(ctx.state_warning())),
                ))]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "Ctrl +",
                        ctx.apply(Style::default().fg(ctx.accent_teal())),
                    )),
                    Cell::from("Zoom in (increase font size)"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "Ctrl -",
                        ctx.apply(Style::default().fg(ctx.accent_teal())),
                    )),
                    Cell::from("Zoom out (decrease font size)"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "Q (shift+q)",
                        ctx.apply(Style::default().fg(ctx.state_error())),
                    )),
                    Cell::from("Quit the application"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "m",
                        ctx.apply(Style::default().fg(ctx.state_selected())),
                    )),
                    Cell::from("Toggle this help screen"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "c",
                        ctx.apply(Style::default().fg(ctx.accent_peach())),
                    )),
                    Cell::from("Open Config screen"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "r",
                        ctx.apply(Style::default().fg(ctx.accent_sapphire())),
                    )),
                    Cell::from("Open RSS screen"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "J",
                        ctx.apply(Style::default().fg(ctx.state_info())),
                    )),
                    Cell::from("Open event journal"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "z",
                        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
                    )),
                    Cell::from("Toggle Zen/Power Saving mode"),
                ]),
                Row::new(vec![Cell::from(""), Cell::from("")]).height(1),
                // --- List Navigation & Sorting ---
                Row::new(vec![Cell::from(Span::styled(
                    "List Navigation",
                    ctx.apply(Style::default().fg(ctx.state_warning())),
                ))]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "↑ / ↓ / k / j",
                        ctx.apply(Style::default().fg(ctx.state_info())),
                    )),
                    Cell::from("Navigate torrents list"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "← / → / h / l",
                        ctx.apply(Style::default().fg(ctx.state_info())),
                    )),
                    Cell::from("Navigate between header columns"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "s",
                        ctx.apply(Style::default().fg(ctx.state_success())),
                    )),
                    Cell::from("Change sort order for the selected column"),
                ]),
                Row::new(vec![Cell::from(""), Cell::from("")]).height(1),
                // --- Torrent Management ---
                Row::new(vec![Cell::from(Span::styled(
                    "Torrent Actions",
                    ctx.apply(Style::default().fg(ctx.state_warning())),
                ))]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "p",
                        ctx.apply(Style::default().fg(ctx.state_success())),
                    )),
                    Cell::from("Pause / Resume selected torrent"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "d / D",
                        ctx.apply(Style::default().fg(ctx.state_error())),
                    )),
                    Cell::from("Delete torrent (D includes downloaded files)"),
                ]),
                Row::new(vec![Cell::from(""), Cell::from("")]).height(1),
                Row::new(vec![Cell::from(Span::styled(
                    "Adding Torrents",
                    ctx.apply(Style::default().fg(ctx.state_warning())),
                ))]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "a",
                        ctx.apply(Style::default().fg(ctx.state_success())),
                    )),
                    Cell::from("Open file picker to add a .torrent file"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "Paste",
                        ctx.apply(Style::default().fg(ctx.accent_sapphire())),
                    )),
                    Cell::from(
                        "Use your terminal paste shortcut to add a magnet link or file path",
                    ),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "CLI",
                        ctx.apply(Style::default().fg(ctx.accent_sapphire())),
                    )),
                    Cell::from("Use `superseedr add ...` from another terminal"),
                ]),
                Row::new(vec![Cell::from(""), Cell::from("")]).height(1),
                // --- Graph Controls ---
                Row::new(vec![Cell::from(Span::styled(
                    "Graph & Panes",
                    ctx.apply(Style::default().fg(ctx.state_warning())),
                ))]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "t / T",
                        ctx.apply(Style::default().fg(ctx.accent_teal())),
                    )),
                    Cell::from("Switch graph time scale forward/backward"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "g / G",
                        ctx.apply(Style::default().fg(ctx.accent_teal())),
                    )),
                    Cell::from("Switch chart panel view forward/backward"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "[ / ]",
                        ctx.apply(Style::default().fg(ctx.accent_teal())),
                    )),
                    Cell::from("Change UI refresh rate (FPS)"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "x",
                        ctx.apply(Style::default().fg(ctx.accent_teal())),
                    )),
                    Cell::from("Anonymize torrent names"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "< / >",
                        ctx.apply(Style::default().fg(ctx.state_selected())),
                    )),
                    Cell::from("Cycle UI theme"),
                ]),
                Row::new(vec![Cell::from(""), Cell::from("")]).height(1),
                // --- Peer Flags Legend ---
                Row::new(vec![
                    // First Cell (for the first column)
                    Cell::from(Span::styled(
                        "Peer Flags Legend",
                        ctx.apply(Style::default().fg(ctx.state_warning())),
                    )),
                    // Second Cell (for the second column)
                    Cell::from(Line::from(vec![
                        // Legend pairing: DL/UL status
                        Span::raw("DL: (You "),
                        Span::styled("■", ctx.apply(Style::default().fg(ctx.accent_sapphire()))),
                        Span::styled("■", ctx.apply(Style::default().fg(ctx.accent_maroon()))),
                        Span::raw(") | UL: (Peer "),
                        Span::styled("■", ctx.apply(Style::default().fg(ctx.accent_teal()))),
                        Span::styled("■", ctx.apply(Style::default().fg(ctx.accent_peach()))),
                        Span::raw(")"),
                    ])),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "■",
                        ctx.apply(Style::default().fg(ctx.accent_sapphire())),
                    )),
                    Cell::from("You are interested (DL Potential)"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "■",
                        ctx.apply(Style::default().fg(ctx.accent_maroon())),
                    )),
                    Cell::from("Peer is choking you (DL Block)"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "■",
                        ctx.apply(Style::default().fg(ctx.accent_teal())),
                    )),
                    Cell::from("Peer is interested (UL Opportunity)"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "■",
                        ctx.apply(Style::default().fg(ctx.accent_peach())),
                    )),
                    Cell::from("You are choking peer (UL Restriction)"),
                ]),
                Row::new(vec![Cell::from(""), Cell::from("")]).height(1),
                Row::new(vec![Cell::from(Span::styled(
                    "Disk Stats Legend",
                    ctx.apply(Style::default().fg(ctx.state_warning())),
                ))]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "↑ (Read)",
                        ctx.apply(Style::default().fg(ctx.state_success())),
                    )),
                    Cell::from("Data read from disk"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "↓ (Write)",
                        ctx.apply(Style::default().fg(ctx.accent_sky())),
                    )),
                    Cell::from("Data written to disk"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "Seek",
                        ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
                    )),
                    Cell::from("Avg. distance between I/O ops (lower is better)"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "Latency",
                        ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
                    )),
                    Cell::from("Time to complete one I/O op (lower is better)"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "IOPS",
                        ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
                    )),
                    Cell::from("I/O Operations Per Second (total workload)"),
                ]),
                Row::new(vec![Cell::from(""), Cell::from("")]).height(1),
                Row::new(vec![Cell::from(Span::styled(
                    "Self-Tuning Legend",
                    ctx.apply(Style::default().fg(ctx.state_warning())),
                ))]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "Best Score",
                        ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
                    )),
                    Cell::from("Score measuring if randomized changes resulted in optimal speeds."),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "Self-Tune (Xs):",
                        ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
                    )),
                    Cell::from("Tuning state with countdown to the next adjustment cycle."),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "Resource Rows",
                        ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
                    )),
                    Cell::from("Current limits shown as numbers for Peers/Reads/Writes/Reserve."),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "(+/-/0)",
                        ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
                    )),
                    Cell::from("Signed change vs best limits (green positive, red negative)."),
                ]),
                Row::new(vec![Cell::from(""), Cell::from("")]).height(1),
                Row::new(vec![Cell::from(Span::styled(
                    "Build Features",
                    ctx.apply(Style::default().fg(ctx.state_warning())),
                ))]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "DHT",
                        ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
                    )),
                    Cell::from(Line::from(vec![
                        #[cfg(feature = "dht")]
                        Span::styled("ON", ctx.apply(Style::default().fg(ctx.state_success()))),
                        #[cfg(not(feature = "dht"))]
                        Span::styled(
                            "Not included in this [PRIVATE] build of superseedr.",
                            ctx.apply(Style::default().fg(ctx.state_error())),
                        ),
                    ])),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "Pex",
                        ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
                    )),
                    Cell::from(Line::from(vec![
                        #[cfg(feature = "pex")]
                        Span::styled("ON", ctx.apply(Style::default().fg(ctx.state_success()))),
                        #[cfg(not(feature = "pex"))]
                        Span::styled(
                            "Not included in this [PRIVATE] build of superseedr.",
                            ctx.apply(Style::default().fg(ctx.state_error())),
                        ),
                    ])),
                ]),
                Row::new(vec![Cell::from(""), Cell::from("")]).height(1),
                // --- NEW: Session Stats at the Bottom ---
                Row::new(vec![Cell::from(Span::styled(
                    "Session Stats",
                    ctx.apply(Style::default().fg(ctx.state_warning())),
                ))]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "Level Up:",
                        ctx.apply(Style::default().fg(ctx.state_selected())),
                    )),
                    Cell::from("Upload data or keep a large library seeding."),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        gauge_str,
                        ctx.apply(Style::default().fg(ctx.state_success())),
                    )),
                    Cell::from(Span::styled(
                        level_text,
                        Style::default().fg(ctx.state_warning()).bold(),
                    )),
                ]),
            ],
        ),
        AppMode::Rss => (
            " Help / RSS ",
            vec![
                Row::new(vec![
                    Cell::from(Span::styled(
                        "Esc / q",
                        ctx.apply(Style::default().fg(ctx.state_error())),
                    )),
                    Cell::from("Exit RSS mode"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "Tab / h",
                        ctx.apply(Style::default().fg(ctx.state_selected())),
                    )),
                    Cell::from("Next pane focus (Tab) / swap Explorer with History (h)"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "s",
                        ctx.apply(Style::default().fg(ctx.state_warning())),
                    )),
                    Cell::from("Sync now"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "↑ / ↓ / k / j",
                        ctx.apply(Style::default().fg(ctx.state_info())),
                    )),
                    Cell::from("Move selection in active RSS sub-screen"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "a / d / Space",
                        ctx.apply(Style::default().fg(ctx.state_complete())),
                    )),
                    Cell::from(
                        "Focused pane actions: Links add/delete/toggle; Filters add/delete/toggle",
                    ),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "Enter",
                        ctx.apply(Style::default().fg(ctx.state_warning())),
                    )),
                    Cell::from("Confirm add/search input"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "/",
                        ctx.apply(Style::default().fg(ctx.accent_sapphire())),
                    )),
                    Cell::from("Start Explorer search mode (when Explorer pane is focused)"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "Y",
                        ctx.apply(Style::default().fg(ctx.state_success())),
                    )),
                    Cell::from("Download selected Explorer item (if not downloaded)"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "j / k / ↑ / ↓",
                        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
                    )),
                    Cell::from("Move selection in the focused pane"),
                ]),
            ],
        ),
        AppMode::Journal => (" Help / Journal ", journal_help_rows(ctx)),
        AppMode::Config => (
            " Help / Config ",
            vec![
                Row::new(vec![
                    Cell::from(Span::styled(
                        "Esc / q",
                        ctx.apply(Style::default().fg(ctx.state_success())),
                    )),
                    Cell::from("Save and exit config"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "↑ / ↓ / k / j",
                        ctx.apply(Style::default().fg(ctx.state_info())),
                    )),
                    Cell::from("Navigate items"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "← / → / h / l",
                        ctx.apply(Style::default().fg(ctx.state_info())),
                    )),
                    Cell::from("Decrease / Increase value"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "Enter",
                        ctx.apply(Style::default().fg(ctx.state_warning())),
                    )),
                    Cell::from("Start or confirm editing"),
                ]),
            ],
        ),
        AppMode::FileBrowser => (
            " Help / File Browser ",
            vec![
                Row::new(vec![
                    Cell::from(Span::styled(
                        "Esc",
                        ctx.apply(Style::default().fg(ctx.state_error())),
                    )),
                    Cell::from("Cancel selection"),
                ]),
                // ... rest of help items ...
            ],
        ),
        _ => (
            " Help ",
            vec![Row::new(vec![Cell::from(
                "No help available for this view.",
            )])],
        ),
    };

    if is_shared_config_mode() && matches!(mode, AppMode::Normal | AppMode::Welcome | AppMode::Help)
    {
        rows.extend([
            Row::new(vec![Cell::from(Span::styled(
                "Cluster Mode",
                ctx.apply(Style::default().fg(ctx.state_warning())),
            ))]),
            Row::new(vec![
                Cell::from(Span::styled(
                    "Leader",
                    ctx.apply(Style::default().fg(ctx.state_success())),
                )),
                Cell::from("Downloads, seeds, and publishes cluster progress"),
            ]),
            Row::new(vec![
                Cell::from(Span::styled(
                    "Follower",
                    ctx.apply(Style::default().fg(ctx.state_info())),
                )),
                Cell::from("Reads leader progress and may seed complete shared data"),
            ]),
            Row::new(vec![Cell::from(""), Cell::from("")]).height(1),
        ]);
    }

    let help_table = Table::new(rows, [Constraint::Length(20), Constraint::Min(30)]).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.border)))
            .padding(Padding::new(2, 2, 1, 1)),
    );

    f.render_widget(Clear, area);
    f.render_widget(help_table, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyEvent, KeyModifiers};

    #[test]
    fn help_esc_returns_to_normal() {
        let mut app_state = AppState {
            mode: AppMode::Help,
            ..Default::default()
        };

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &mut app_state,
        );

        assert!(matches!(app_state.mode, AppMode::Normal));
    }

    #[test]
    fn help_m_press_returns_to_normal() {
        let mut app_state = AppState {
            mode: AppMode::Help,
            ..Default::default()
        };

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE)),
            &mut app_state,
        );

        assert!(matches!(app_state.mode, AppMode::Normal));
    }

    #[test]
    fn help_ignores_non_close_key() {
        let mut app_state = AppState {
            mode: AppMode::Help,
            ..Default::default()
        };

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            &mut app_state,
        );

        assert!(matches!(app_state.mode, AppMode::Help));
    }

    #[test]
    fn help_handler_ignores_when_not_in_help_mode() {
        let mut app_state = AppState {
            mode: AppMode::Normal,
            ..Default::default()
        };

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &mut app_state,
        );

        assert!(matches!(app_state.mode, AppMode::Normal));
    }

    #[test]
    fn help_footer_includes_cluster_entries_when_present() {
        let settings = Settings::default();
        let app_state = AppState {
            cluster_role_label: Some("Leader".to_string()),
            cluster_runtime_label: Some("Reader".to_string()),
            ..Default::default()
        };

        let entries = build_help_footer_entries(&settings, &app_state);

        assert!(entries.contains(&("Cluster", "Leader".to_string())));
        assert!(entries.contains(&("Runtime", "Reader".to_string())));
    }

    #[test]
    fn help_footer_omits_cluster_entries_when_absent() {
        let settings = Settings::default();
        let app_state = AppState::default();

        let entries = build_help_footer_entries(&settings, &app_state);

        assert!(!entries.iter().any(|(label, _)| *label == "Cluster"));
        assert!(!entries.iter().any(|(label, _)| *label == "Runtime"));
    }
}
