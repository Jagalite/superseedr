// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use ratatui::symbols::Marker;
use ratatui::{prelude::*, widgets::*};

use crate::tui_formatters::*;

use crate::app::GraphDisplayMode;

use crate::app::{
    AppMode, AppState, ConfigItem, SelectedHeader, TorrentControlState, PEER_HEADERS,
    TORRENT_HEADERS,
};

use crate::config::get_app_paths;

use crate::config::{PeerSortColumn, Settings, SortDirection, TorrentSortColumn};

use crate::theme;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::time::{SystemTime, UNIX_EPOCH};

static APP_VERSION: &str = env!("CARGO_PKG_VERSION");

pub const SECONDS_HISTORY_MAX: usize = 3600; // 1 hour of per-second data
pub const MINUTES_HISTORY_MAX: usize = 48 * 60; // 48 hours of per-minute data

pub fn draw(f: &mut Frame, app_state: &AppState, settings: &Settings) {
    if app_state.show_help {
        draw_help_popup(f, app_state, &app_state.mode);
        return;
    }

    match &app_state.mode {
        AppMode::Welcome => {
            draw_welcome_screen(f);
            return;
        }
        AppMode::PowerSaving => {
            draw_power_saving_screen(f, app_state, settings);
            return;
        }
        AppMode::ConfigPathPicker {
            file_explorer,
            for_item,
            ..
        } => {
            let area = centered_rect(80, 70, f.area());
            f.render_widget(Clear, area);
            let block = Block::default()
                .title(Span::styled(
                    format!("Select a Folder - {:?}", for_item),
                    Style::default().fg(theme::MAUVE),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::SURFACE2));

            let inner_area = block.inner(area);

            let chunks =
                Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner_area);

            let explorer_area = chunks[0];
            let footer_area = chunks[1];

            let footer_text = Line::from(vec![
                Span::styled("[Tab]", Style::default().fg(theme::GREEN)),
                Span::raw(" Confirm | "),
                Span::styled("[Esc]", Style::default().fg(theme::RED)),
                Span::raw(" Cancel | "),
                Span::styled("←→↑↓", Style::default().fg(theme::BLUE)),
                Span::raw(" Navigate"),
            ])
            .alignment(Alignment::Center);

            let footer_paragraph =
                Paragraph::new(footer_text).style(Style::default().fg(theme::SUBTEXT1));

            f.render_widget(block, area);
            f.render_widget(&file_explorer.widget(), explorer_area);
            f.render_widget(footer_paragraph, footer_area);
            return;
        }
        AppMode::Config {
            settings_edit,
            selected_index,
            items,
            editing,
        } => {
            draw_config_screen(f, settings_edit, *selected_index, items, editing);
            return;
        }
        AppMode::FilePicker(file_explorer) => {
            let area = centered_rect(80, 70, f.area());
            f.render_widget(Clear, area);

            let block = Block::default()
                .title(Span::styled(
                    "Select Download Folder",
                    Style::default().fg(theme::MAUVE),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::SURFACE2));

            let inner_area = block.inner(area);

            let chunks =
                Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner_area);

            let explorer_area = chunks[0];
            let footer_area = chunks[1];

            let footer_text = Line::from(vec![
                Span::styled("[Tab]", Style::default().fg(theme::GREEN)),
                Span::raw(" Confirm | "),
                Span::styled("[Esc]", Style::default().fg(theme::RED)),
                Span::raw(" Cancel | "),
                Span::styled("←→↑↓", Style::default().fg(theme::BLUE)),
                Span::raw(" Navigate"),
            ])
            .alignment(Alignment::Center);

            let footer_paragraph =
                Paragraph::new(footer_text).style(Style::default().fg(theme::SUBTEXT1));

            f.render_widget(block, area);
            f.render_widget(&file_explorer.widget(), explorer_area);
            f.render_widget(footer_paragraph, footer_area);
            return;
        }
        AppMode::DeleteConfirm { .. } => {
            draw_delete_confirm_dialog(f, app_state);
            return;
        }
        _ => {}
    }

    let main_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(10),
            Constraint::Length(27),
            Constraint::Length(1),
        ])
        .split(f.area());

    let top_chunk = main_layout[0];
    let bottom_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(80), // 75% for the chart
            Constraint::Percentage(20), // 25% for the new stats panel
        ])
        .split(main_layout[1]); // Split the original bottom chunk

    let chart_chunk = bottom_chunks[0];
    let stats_chunk = bottom_chunks[1];
    let footer_chunk = main_layout[2];

    let top_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(top_chunk);

    let left_pane = top_chunks[0];
    let right_pane = top_chunks[1];

    let right_pane_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(9), // Fixed height of 9 rows for the Details section
            Constraint::Min(0),     // The rest of the space will be for the Peers table
        ])
        .split(right_pane);
    let details_chunk = right_pane_chunks[0]; // Top part for details
    let peers_chunk = right_pane_chunks[1]; // Bottom part for peers

    draw_left_pane(f, app_state, left_pane);

    draw_right_pane(f, app_state, details_chunk, peers_chunk);

    draw_network_chart(f, app_state, chart_chunk);

    draw_stats_panel(f, app_state, settings, stats_chunk);

    draw_footer(f, app_state, settings, footer_chunk);

    if let Some(error_text) = &app_state.system_error {
        draw_status_error_popup(f, error_text);
    }

    if app_state.should_quit {
        draw_shutdown_screen(f, app_state);
    }
}

fn draw_delete_confirm_dialog(f: &mut Frame, app_state: &AppState) {
    if let AppMode::DeleteConfirm {
        info_hash,
        with_files,
    } = &app_state.mode
    {
        if let Some(torrent_to_delete) = app_state.torrents.get(info_hash) {
            let area = centered_rect(50, 25, f.area());
            f.render_widget(Clear, area);

            let torrent_name = &torrent_to_delete.latest_state.torrent_name;
            let download_path_str = torrent_to_delete
                .latest_state
                .download_path
                .to_string_lossy();

            let mut text = vec![
                Line::from(Span::styled(
                    "Confirm Deletion",
                    Style::default().fg(theme::RED),
                )),
                Line::from(""),
                Line::from(torrent_name.as_str()),
                Line::from(Span::styled(
                    download_path_str.to_string(),
                    Style::default().fg(theme::SUBTEXT1),
                )),
                Line::from(""), // Spacer
            ];

            if *with_files {
                // Message for [D] - Delete with files
                text.push(Line::from("Are you sure you want to remove this torrent?"));
                text.push(Line::from("")); // Add a blank line for spacing
                text.push(Line::from(Span::styled(
                    "This will also permanently delete associated files.",
                    Style::default().fg(theme::YELLOW).bold().underlined(),
                )));
            } else {
                // Message for [d] - Delete torrent only
                text.push(Line::from("Are you sure you want to remove this torrent?"));
                text.push(Line::from(""));
                text.push(Line::from(vec![
                    Span::raw("The downloaded files will "),
                    Span::styled(
                        "NOT",
                        Style::default().fg(theme::YELLOW).bold().underlined(),
                    ),
                    Span::raw(" be deleted."),
                ]));
                text.push(Line::from(""));
                text.push(Line::from(vec![
                    //
                    Span::styled("Press ", Style::default().fg(theme::SUBTEXT1)),
                    Span::styled("[D]", Style::default().fg(theme::YELLOW).bold()),
                    Span::styled(
                        " instead to remove the torrent and delete associated files.",
                        Style::default().fg(theme::SUBTEXT1),
                    ),
                ]));
            }

            text.push(Line::from(""));
            text.push(Line::from(vec![
                Span::styled("[Enter]", Style::default().fg(theme::GREEN)),
                Span::raw(" Confirm  "),
                Span::styled("[Esc]", Style::default().fg(theme::RED)),
                Span::raw(" Cancel"),
            ]));
            // --- END FIX ---

            let block = Block::default()
                .title("Confirmation")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::SURFACE2));

            let paragraph = Paragraph::new(text)
                .block(block)
                .style(Style::default().fg(theme::TEXT));
            f.render_widget(paragraph, area);
        }
    }
}

fn draw_left_pane(f: &mut Frame, app_state: &AppState, left_pane: Rect) {
    let mut table_state = TableState::default();
    if matches!(app_state.selected_header, SelectedHeader::Torrent(_)) {
        table_state.select(Some(app_state.selected_torrent_index));
    }

    let has_unfinished_torrents = app_state.torrents.values().any(|t| {
        let state = &t.latest_state;
        state.number_of_pieces_total > 0
            && state.number_of_pieces_completed < state.number_of_pieces_total
    });

    let (widths, name_column_index): (Vec<Constraint>, usize) = if has_unfinished_torrents {
        (
            vec![
                Constraint::Length(7),      // Progress
                Constraint::Percentage(65), // Name
                Constraint::Percentage(15), // DL
                Constraint::Percentage(15), // UL
            ],
            1,
        )
    } else {
        (
            vec![
                Constraint::Percentage(70),
                Constraint::Percentage(15),
                Constraint::Percentage(15),
            ],
            0,
        )
    };

    let table_block = Block::default().borders(Borders::ALL);
    let table_inner_area = table_block.inner(left_pane);
    let column_spacing = 1; // This is ratatui's default.
    let total_spacing = (widths.len().saturating_sub(1) * column_spacing as usize) as u16;
    let content_width = table_inner_area.width.saturating_sub(total_spacing);
    let temp_layout_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(widths.clone())
        .split(Rect::new(0, 0, content_width, 1)); // A dummy rect of the correct width
    let name_column_width = temp_layout_chunks[name_column_index].width as usize;

    let header_cells: Vec<Cell> = {
        let mut cells: Vec<Cell> = TORRENT_HEADERS
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let is_selected = app_state.selected_header == SelectedHeader::Torrent(i);
                let (sort_col, sort_dir) = app_state.torrent_sort;
                let is_sorting_by_this = sort_col == *h;
                let text = match h {
                    TorrentSortColumn::Name => "Name",
                    TorrentSortColumn::Down => "DL",
                    TorrentSortColumn::Up => "UL",
                };
                let mut text_with_indicator = text.to_string();
                let mut style = Style::default().fg(theme::YELLOW);
                if is_sorting_by_this {
                    style = style.fg(theme::MAUVE);
                    let indicator = if sort_dir == SortDirection::Ascending {
                        " ▲"
                    } else {
                        " ▼"
                    };
                    text_with_indicator.push_str(indicator);
                }
                let mut text_span = Span::styled(text, style);
                if is_selected {
                    text_span = text_span.underlined().bold();
                }
                let mut spans = vec![text_span];
                if is_sorting_by_this {
                    let indicator = if sort_dir == SortDirection::Ascending {
                        " ▲"
                    } else {
                        " ▼"
                    };
                    spans.push(Span::styled(indicator, style));
                }
                Cell::from(Line::from(spans))
            })
            .collect();
        if has_unfinished_torrents {
            cells.insert(
                0,
                Cell::from(Span::styled("Done", Style::default().fg(theme::YELLOW))),
            );
        }
        cells
    };
    let header = Row::new(header_cells).height(1);

    let rows = app_state
        .torrent_list_order
        .iter()
        .enumerate()
        .map(|(i, info_hash)| {
            match app_state.torrents.get(info_hash) {
                Some(torrent) => {
                    let state = &torrent.latest_state;
                    let progress = if state.number_of_pieces_total > 0 {
                        (state.number_of_pieces_completed as f64 / state.number_of_pieces_total as f64) * 100.0
                    } else {
                        0.0
                    };
                    let progress_style = Style::default().fg(theme::TEXT);

                    let is_selected = i == app_state.selected_torrent_index;

                    let mut row_style = match state.torrent_control_state {
                        TorrentControlState::Running => Style::default().fg(theme::TEXT),
                        TorrentControlState::Paused => Style::default().fg(theme::SURFACE1),
                        TorrentControlState::Deleting => Style::default().fg(theme::RED),
                    };
                    row_style = if state.torrent_control_state == TorrentControlState::Deleting {
                        row_style.fg(theme::OVERLAY0)
                    } else {
                        row_style
                    };

                    let name_to_display = if app_state.anonymize_torrent_names {
                        format!("Torrent {}", i + 1)
                    } else {
                        state.torrent_name.clone()
                    };

                    let mut name_cell =
                        Cell::from(truncate_with_ellipsis(&name_to_display, name_column_width));
                    if is_selected {
                        name_cell = name_cell.style(Style::default().fg(theme::YELLOW));
                        row_style = row_style.add_modifier(Modifier::BOLD);
                    }

                    let mut row_cells = vec![
                        name_cell,
                        Cell::from(format_speed(torrent.smoothed_download_speed_bps))
                            .style(speed_to_style(torrent.smoothed_download_speed_bps)),
                        Cell::from(format_speed(torrent.smoothed_upload_speed_bps))
                            .style(speed_to_style(torrent.smoothed_upload_speed_bps)),
                    ];

                    if has_unfinished_torrents {
                        row_cells.insert(0, Cell::from(format!("{:.1}%", progress)).style(progress_style));
                    }

                    Row::new(row_cells).style(row_style)
                }
                None => {
                    // This case should ideally not happen if the state is consistent.
                    // Return an empty or placeholder row.
                    Row::new(vec![
                        Cell::from(""),
                        Cell::from("Missing torrent data..."),
                        Cell::from(""),
                        Cell::from(""),
                        Cell::from(""),
                    ])
                }
            }
        });

    let border_style = if matches!(app_state.selected_header, SelectedHeader::Torrent(_)) {
        Style::default().fg(theme::MAUVE) // Active color
    } else {
        Style::default().fg(theme::SURFACE2) // Inactive color
    };

    let title_content = if app_state.is_searching {
        // State 1: Actively searching
        Line::from(vec![
            Span::raw(" Filter: /"),
            Span::styled(
                app_state.search_query.clone(),
                Style::default().fg(theme::YELLOW),
            ),
            Span::raw(" "), // Space for cursor
        ])
    } else if !app_state.search_query.is_empty() {
        // State 2: Filter is active (Sleek version)
        Line::from(vec![
            Span::styled("Torrents ", Style::default().fg(theme::GREEN)),
            Span::styled("[", Style::default().fg(theme::SUBTEXT1)), // Grey bracket
            Span::styled(
                app_state.search_query.clone(),
                Style::default()
                    .fg(theme::SUBTEXT1) // Greyed out
                    .add_modifier(Modifier::ITALIC), // Italic
            ),
            Span::styled("]", Style::default().fg(theme::SUBTEXT1)), // Grey bracket
        ])
    } else {
        // State 3: Normal
        Line::from(Span::styled("Torrents", Style::default().fg(theme::GREEN)))
    };

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(title_content)
        )
        .row_highlight_style(Style::default().add_modifier(Modifier::BOLD));

    if app_state.is_searching {
        f.set_cursor_position(Position {
            x: left_pane.x + 1 + 10 + app_state.search_query.len() as u16,
            y: left_pane.y, // The title is on the top border
        });
    }
    f.render_stateful_widget(table, left_pane, &mut table_state);
}

fn draw_network_chart(f: &mut Frame, app_state: &AppState, chart_chunk: Rect) {
    let smooth_data = |data: &[u64], alpha: f64| -> Vec<u64> {
        if data.is_empty() {
            return Vec::new();
        }
        let mut smoothed_data = Vec::with_capacity(data.len());
        let mut last_ema = data[0] as f64;
        smoothed_data.push(last_ema as u64);
        for &value in data.iter().skip(1) {
            let current_ema = (value as f64 * alpha) + (last_ema * (1.0 - alpha));
            smoothed_data.push(current_ema as u64);
            last_ema = current_ema;
        }
        smoothed_data
    };

    // 1. Calculate stable Y-axis for network speed
    let stable_max_speed = app_state
        .avg_download_history
        .iter()
        .chain(app_state.avg_upload_history.iter())
        .max()
        .copied()
        .unwrap_or(10_000);
    let nice_max_speed = calculate_nice_upper_bound(stable_max_speed);

    // 2. Select correct data sources including backoff history
    let (
        dl_history_source,
        ul_history_source,
        backoff_history_source_ms, // <-- Added backoff source
        time_window_points,
        _time_unit_secs, // Used for debugging or potential future features
    ) = match app_state.graph_mode {
        GraphDisplayMode::ThreeHours
        | GraphDisplayMode::TwelveHours
        | GraphDisplayMode::TwentyFourHours => {
            let max_points = MINUTES_HISTORY_MAX; // Use the constant defined in app.rs
            (
                &app_state.minute_avg_dl_history,
                &app_state.minute_avg_ul_history,
                &app_state.minute_disk_backoff_history_ms, // Use minute backoff history
                max_points, // Max points based on minute history capacity
                60,
            )
        }
        _ => {
            let points = app_state.graph_mode.as_seconds().min(SECONDS_HISTORY_MAX); // Use constant
            (
                &app_state.avg_download_history,
                &app_state.avg_upload_history,
                &app_state.disk_backoff_history_ms, // Use second backoff history
                points, // Points based on graph mode, capped by history capacity
                1,
            )
        }
    };

    // 3. Get relevant slices based on the *actual* available data and window size
    let dl_len = dl_history_source.len();
    let ul_len = ul_history_source.len();
    let backoff_len = backoff_history_source_ms.len();

    // Use the *minimum* length of available history for slicing to avoid panics
    let available_points = dl_len.min(ul_len).min(backoff_len);
    let points_to_show = time_window_points.min(available_points); // Don't try to show more points than available

    let dl_history_slice = &dl_history_source[dl_len.saturating_sub(points_to_show)..];
    let ul_history_slice = &ul_history_source[ul_len.saturating_sub(points_to_show)..];

    let skip_count = backoff_len.saturating_sub(points_to_show);
    let backoff_history_relevant_ms: Vec<u64> = backoff_history_source_ms
        .iter()
        .skip(skip_count)
        .copied() // Copy the u64 values out of the iterator
        .collect();

    // 4. Create datasets
    let smoothing_period = 5.0;
    let alpha = 2.0 / (smoothing_period + 1.0);
    let smoothed_dl_data = smooth_data(dl_history_slice, alpha); // We need the smoothed DL data
    let smoothed_ul_data = smooth_data(ul_history_slice, alpha);

    let dl_data: Vec<(f64, f64)> = smoothed_dl_data
        .iter()
        .enumerate()
        .map(|(i, &s)| (i as f64, s as f64))
        .collect();
    let ul_data: Vec<(f64, f64)> = smoothed_ul_data
        .iter()
        .enumerate()
        .map(|(i, &s)| (i as f64, s as f64))
        .collect();

    // Map backoff occurrences to the Y-value of the download speed at that time.
    let backoff_marker_data: Vec<(f64, f64)> = backoff_history_relevant_ms // <-- Use the new Vec
        .iter() // Now iter() works correctly on the Vec
        .enumerate()
        .filter_map(|(i, &ms)| {
            if ms > 0 {
                // If a backoff occurred in this interval
                // Find the corresponding DL speed Y-value using smoothed data
                // Ensure index 'i' is valid for smoothed_dl_data as well
                let y_val = smoothed_dl_data.get(i).copied().unwrap_or(0) as f64;
                Some((i as f64, y_val)) // Plot at (index, dl_speed)
            } else {
                None // Don't plot anything if no backoff occurred
            }
        })
        .collect();

    let backoff_dataset = Dataset::default()
        .name("File Limits") // Keep the name for legend
        .marker(Marker::Braille) // Use dots
        .graph_type(GraphType::Scatter) // Only draw markers
        .style(Style::default().fg(theme::RED).add_modifier(Modifier::BOLD)) // Red color
        .data(&backoff_marker_data);

    let datasets = vec![
        Dataset::default() // DL Data
            .name("Download")
            .marker(Marker::Braille)
            .style(
                Style::default()
                    .fg(theme::BLUE)
                    .add_modifier(Modifier::BOLD),
            )
            .data(&dl_data),
        Dataset::default() // UL Data
            .name("Upload")
            .marker(Marker::Braille)
            .style(
                Style::default()
                    .fg(theme::GREEN)
                    .add_modifier(Modifier::BOLD),
            )
            .data(&ul_data),
        backoff_dataset, // Add the backoff markers dataset
    ];

    // 5. Create labels for axes
    let y_speed_axis_labels = vec![
        Span::raw("0"),
        Span::styled(
            format_speed(nice_max_speed / 2),
            Style::default().fg(theme::SUBTEXT0),
        ),
        Span::styled(
            format_speed(nice_max_speed),
            Style::default().fg(theme::SUBTEXT0),
        ),
    ];
    let x_labels = generate_x_axis_labels(app_state.graph_mode);

    // 6. Create the Chart (Using only ONE Y-axis)
    let all_modes = [
        GraphDisplayMode::OneMinute,
        GraphDisplayMode::FiveMinutes,
        GraphDisplayMode::TenMinutes,
        GraphDisplayMode::ThirtyMinutes,
        GraphDisplayMode::OneHour,
        GraphDisplayMode::ThreeHours,
        GraphDisplayMode::TwelveHours,
        GraphDisplayMode::TwentyFourHours,
    ];
    let mut title_spans: Vec<Span> = vec![Span::styled(
        "Network Activity ",
        Style::default().fg(theme::PEACH),
    )];
    for (i, &mode) in all_modes.iter().enumerate() {
        let is_active = mode == app_state.graph_mode;
        let mode_str = mode.to_string();

        let style = if is_active {
            Style::default()
                .fg(theme::YELLOW)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::SURFACE0)
        };

        title_spans.push(Span::styled(mode_str, style));

        if i < all_modes.len().saturating_sub(1) {
            title_spans.push(Span::styled(" ", Style::default().fg(theme::SURFACE2)));
        }
    }
    let chart_title = Line::from(title_spans);

    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .title(chart_title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::SURFACE2)),
        )
        .x_axis(
            Axis::default()
                .style(Style::default().fg(theme::OVERLAY0))
                .bounds([0.0, points_to_show.saturating_sub(1) as f64]) // Use actual points shown
                .labels(x_labels),
        )
        .y_axis(
            // Single Y-axis for Speed
            Axis::default()
                .style(Style::default().fg(theme::OVERLAY0))
                .bounds([0.0, nice_max_speed as f64])
                .labels(y_speed_axis_labels),
        )
        .legend_position(Some(LegendPosition::TopRight)); // Optional: Show legend

    f.render_widget(chart, chart_chunk);
}

fn draw_stats_panel(f: &mut Frame, app_state: &AppState, settings: &Settings, stats_chunk: Rect) {
    let total_peers = app_state
        .torrents
        .values()
        .map(|t| t.latest_state.number_of_successfully_connected_peers)
        .sum::<usize>();

    let dl_speed = *app_state.avg_download_history.last().unwrap_or(&0);
    let dl_limit = settings.global_download_limit_bps;

    let mut dl_spans = vec![
        Span::styled("DL Speed: ", Style::default().fg(theme::SKY)),
        Span::raw(format_speed(dl_speed)),
        Span::raw(" / "),
    ];
    if dl_limit > 0 && dl_speed >= dl_limit {
        dl_spans.push(Span::styled(
            format_limit_bps(dl_limit),
            Style::default().fg(theme::RED),
        ));
    } else {
        dl_spans.push(Span::styled(
            format_limit_bps(dl_limit),
            Style::default().fg(theme::SUBTEXT0),
        ));
    }

    let ul_speed = *app_state.avg_upload_history.last().unwrap_or(&0);
    let ul_limit = settings.global_upload_limit_bps;

    let mut ul_spans = vec![
        Span::styled("UL Speed: ", Style::default().fg(theme::GREEN)),
        Span::raw(format_speed(ul_speed)),
        Span::raw(" / "),
    ];

    if ul_limit > 0 && ul_speed >= ul_limit {
        // Throttling: show limit in Red
        ul_spans.push(Span::styled(
            format_limit_bps(ul_limit),
            Style::default().fg(theme::RED),
        ));
    } else {
        // Not throttling or unlimited: show limit in a subtle color
        ul_spans.push(Span::styled(
            format_limit_bps(ul_limit),
            Style::default().fg(theme::SUBTEXT0),
        ));
    }

    let stats_text = vec![
        Line::from(vec![
            Span::styled("Run Time: ", Style::default().fg(theme::TEAL)),
            Span::raw(format_time(app_state.run_time)),
        ]),
        Line::from(vec![
            Span::styled("Torrents: ", Style::default().fg(theme::PEACH)),
            Span::raw(app_state.torrents.len().to_string()),
        ]),
        Line::from(""),
        Line::from(dl_spans),
        Line::from(vec![
            Span::styled("Session DL: ", Style::default().fg(theme::SKY)),
            Span::raw(format_bytes(app_state.session_total_downloaded)),
        ]),
        Line::from(vec![
            Span::styled("Lifetime DL: ", Style::default().fg(theme::SKY)),
            // --- CHANGE THIS LINE ---
            Span::raw(format_bytes(
                app_state.lifetime_downloaded_from_config + app_state.session_total_downloaded,
            )),
        ]),
        Line::from(""),
        Line::from(ul_spans),
        Line::from(vec![
            Span::styled("Session UL: ", Style::default().fg(theme::GREEN)),
            Span::raw(format_bytes(app_state.session_total_uploaded)),
        ]),
        Line::from(vec![
            Span::styled("Lifetime UL: ", Style::default().fg(theme::GREEN)),
            Span::raw(format_bytes(
                app_state.lifetime_uploaded_from_config + app_state.session_total_uploaded,
            )),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("CPU: ", Style::default().fg(theme::RED)),
            Span::raw(format!("{:.1}%", app_state.cpu_usage)),
        ]),
        Line::from(vec![
            Span::styled("RAM: ", Style::default().fg(theme::YELLOW)),
            Span::raw(format!("{:.1}%", app_state.ram_usage_percent)),
        ]),
        Line::from(vec![
            Span::styled("App RAM: ", Style::default().fg(theme::FLAMINGO)),
            Span::raw(format_memory(app_state.app_ram_usage)),
        ]),
        Line::from(vec![
            Span::styled("Disk    ", Style::default().fg(theme::TEXT)),
            Span::styled("↑ ", Style::default().fg(theme::GREEN)), // Read is now UP arrow, GREEN
            Span::styled(
                format!("{:<12}", format_speed(app_state.avg_disk_read_bps)),
                Style::default().fg(theme::GREEN),
            ),
            Span::styled("↓ ", Style::default().fg(theme::SKY)), // Write is now DOWN arrow, SKY
            Span::styled(
                format_speed(app_state.avg_disk_write_bps),
                Style::default().fg(theme::SKY),
            ),
        ]),
        // Seek Distance (Thrash)
        Line::from(vec![
            Span::styled("Seek    ", Style::default().fg(theme::TEXT)),
            Span::styled("↑ ", Style::default().fg(theme::GREEN)), // Read is UP, GREEN
            Span::styled(
                format!(
                    "{:<12}",
                    format_bytes(app_state.global_disk_read_thrash_score)
                ),
                Style::default().fg(theme::GREEN),
            ),
            Span::styled("↓ ", Style::default().fg(theme::SKY)), // Write is DOWN, SKY
            Span::styled(
                format_bytes(app_state.global_disk_write_thrash_score),
                Style::default().fg(theme::SKY),
            ),
        ]),
        // Latency (Responsiveness)
        Line::from(vec![
            Span::styled("Latency ", Style::default().fg(theme::TEXT)),
            Span::styled("↑ ", Style::default().fg(theme::GREEN)), // Read is UP, GREEN
            Span::styled(
                format!("{:<12}", format_latency(app_state.avg_disk_read_latency)),
                Style::default().fg(theme::GREEN),
            ),
            Span::styled("↓ ", Style::default().fg(theme::SKY)), // Write is DOWN, SKY
            Span::styled(
                format_latency(app_state.avg_disk_write_latency),
                Style::default().fg(theme::SKY),
            ),
        ]),
        // IOPS (Workload)
        Line::from(vec![
            Span::styled("IOPS    ", Style::default().fg(theme::TEXT)),
            Span::styled("↑ ", Style::default().fg(theme::GREEN)), // Read is UP, GREEN
            Span::styled(
                format!("{:<12}", format_iops(app_state.read_iops)),
                Style::default().fg(theme::GREEN),
            ),
            Span::styled("↓ ", Style::default().fg(theme::SKY)), // Write is DOWN, SKY
            Span::styled(
                format_iops(app_state.write_iops),
                Style::default().fg(theme::SKY),
            ),
        ]),
        Line::from(""), // Separator
        Line::from(vec![
            Span::styled("Tune: ", Style::default().fg(theme::TEAL)),
            Span::raw(app_state.last_tuning_score.to_string()),
            Span::styled(" | ", Style::default().fg(theme::SURFACE2)),
            Span::styled("Next in ", Style::default().fg(theme::TEXT)),
            Span::raw(format!("{}s", app_state.tuning_countdown)),
        ]),
        Line::from(vec![
            Span::styled("Thrash: ", Style::default().fg(theme::TEAL)),
            Span::styled(
                format!("{:.1}", app_state.global_disk_thrash_score), // Current
                Style::default().fg(theme::TEXT),
            ),
            Span::styled(" / ", Style::default().fg(theme::SURFACE2)),
            Span::styled(
                format!("{:.1}", app_state.adaptive_max_scpb), // Max
                Style::default().fg(theme::SUBTEXT0),
            ),
        ]),
        Line::from(vec![
            Span::styled("Reserve Pool:  ", Style::default().fg(theme::TEAL)), // Using TEAL for a different color
            Span::raw(app_state.limits.reserve_permits.to_string()),
            format_limit_delta(
                app_state.limits.reserve_permits,
                app_state.last_tuning_limits.reserve_permits,
            ),
        ]),
        {
            let mut spans = format_permits_spans(
                "Peer Slots: ",
                total_peers,
                app_state.limits.max_connected_peers,
                theme::MAUVE,
            );
            spans.push(format_limit_delta(
                app_state.limits.max_connected_peers,
                app_state.last_tuning_limits.max_connected_peers,
            ));
            Line::from(spans)
        },
        Line::from(vec![
            Span::styled("Disk Reads:    ", Style::default().fg(theme::GREEN)),
            Span::raw(app_state.limits.disk_read_permits.to_string()),
            format_limit_delta(
                app_state.limits.disk_read_permits,
                app_state.last_tuning_limits.disk_read_permits,
            ),
        ]),
        Line::from(vec![
            Span::styled("Disk Writes:   ", Style::default().fg(theme::SKY)),
            Span::raw(app_state.limits.disk_write_permits.to_string()),
            format_limit_delta(
                app_state.limits.disk_write_permits,
                app_state.last_tuning_limits.disk_write_permits,
            ),
        ]),
    ];

    let stats_paragraph = Paragraph::new(stats_text)
        .block(
            Block::default()
                .title("Stats")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::SURFACE2)),
        )
        .style(Style::default().fg(theme::TEXT));

    f.render_widget(stats_paragraph, stats_chunk);
}

fn draw_right_pane(f: &mut Frame, app_state: &AppState, details_chunk: Rect, peers_chunk: Rect) {
    if let Some(info_hash) = app_state
        .torrent_list_order
        .get(app_state.selected_torrent_index)
    {
        if let Some(torrent) = app_state.torrents.get(info_hash) {
            let state = &torrent.latest_state;

            let details_chunks = Layout::horizontal([
                Constraint::Percentage(20), // Left side for text
                Constraint::Percentage(80), // Right side for sparkline
            ])
            .split(details_chunk);

            let properties_chunk = details_chunks[0];
            let sparkline_chunk = details_chunks[1];

            let details_block = Block::default()
                .title(Span::styled("Details", Style::default().fg(theme::MAUVE)))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::SURFACE2));
            let details_inner_chunk = details_block.inner(properties_chunk);
            f.render_widget(details_block, properties_chunk);

            // 1. Define a vertical layout with a row for each piece of information.
            let detail_rows = Layout::vertical([
                Constraint::Length(1), // Progress Gauge
                Constraint::Length(1), // Status
                Constraint::Length(1), // Peers
                Constraint::Length(1), // Written / Size
                Constraint::Length(1), // Pieces
                Constraint::Length(1), // ETA
                Constraint::Length(1),
            ])
            .split(details_inner_chunk);

            // --- Render each piece of info as a Paragraph in its own Rect ---
            let progress_chunks = Layout::horizontal([
                Constraint::Length(11), // Fixed width for "Progress: " label
                Constraint::Min(0),     // The rest of the space for the bar and percentage
            ])
            .split(detail_rows[0]);

            f.render_widget(Paragraph::new("Progress: "), progress_chunks[0]);

            let (progress_ratio, progress_label_text) = if state.activity_message.contains("Validating local files...") {
                let ratio = if state.number_of_pieces_total > 0 {
                    state.number_of_pieces_completed as f64 / state.number_of_pieces_total as f64
                } else {
                    0.0
                };
                // For validation, we want to show progress counting down from 100%
                (1.0 - ratio, format!("{:.1}%", (1.0 - ratio) * 100.0))
            } else if state.number_of_pieces_total > 0 {
                let ratio = state.number_of_pieces_completed as f64 / state.number_of_pieces_total as f64;
                (ratio, format!("{:.1}%", ratio * 100.0))
            } else {
                (0.0, "0.0%".to_string())
            };
            let custom_line_set = symbols::line::Set {
                horizontal: "⣿",
                ..symbols::line::THICK
            };
            let line_gauge = LineGauge::default()
                .ratio(progress_ratio)
                .label(progress_label_text)
                .line_set(custom_line_set)
                .filled_style(Style::default().fg(theme::GREEN));
            f.render_widget(line_gauge, progress_chunks[1]);

            // Status
            let status_text = if state.activity_message.is_empty() {
                "Waiting..." // Default text
            } else {
                state.activity_message.as_str() // Use the message if available
            };
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("Status:   ", Style::default().fg(theme::TEXT)),
                    Span::raw(status_text), // Use the determined text
                ])),
                detail_rows[1],
            );

            // Peers
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("Peers:    ", Style::default().fg(theme::TEXT)),
                    Span::raw(state.number_of_successfully_connected_peers.to_string()),
                ])),
                detail_rows[2],
            );

            // Written / Size
            let written_size_spans = if state.number_of_pieces_completed < state.number_of_pieces_total {
                // Downloading
                vec![
                    Span::styled("Written:  ", Style::default().fg(theme::TEXT)),
                    Span::raw(format_bytes(state.bytes_written)),
                    Span::raw(format!(" / {}", format_bytes(state.total_size))),
                ]
            } else {
                // Completed
                vec![
                    Span::styled("Size:     ", Style::default().fg(theme::TEXT)),
                    Span::raw(format_bytes(state.total_size)),
                ]
            };
            f.render_widget(
                Paragraph::new(Line::from(written_size_spans)),
                detail_rows[3],
            );

            // Pieces
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("Pieces:   ", Style::default().fg(theme::TEXT)),
                    Span::raw(format!(
                        "{}/{}",
                        state.number_of_pieces_completed, state.number_of_pieces_total
                    )),
                ])),
                detail_rows[4],
            );

            // ETA
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("ETA:      ", Style::default().fg(theme::TEXT)),
                    Span::raw(format_duration(state.eta)),
                ])),
                detail_rows[5],
            );

            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("Announce: ", Style::default().fg(theme::TEXT)),
                    Span::raw(format_countdown(state.next_announce_in)),
                ])),
                detail_rows[6],
            );

            // --- RENDER PEERS TABLE (in `peers_chunk`) ---
            let mut sorted_peers = state.peers.clone();
            let (sort_by, sort_direction) = app_state.peer_sort;
            sorted_peers.sort_by(|a, b| {
                let ordering = match sort_by {
                    PeerSortColumn::Flags => {
                        let mut a_score = 0;
                        if !a.peer_choking {
                            a_score += 2;
                        } // Priority for peers we can download from.
                        if !a.am_choking {
                            a_score += 1;
                        } // Secondary priority for peers we upload to.

                        let mut b_score = 0;
                        if !b.peer_choking {
                            b_score += 2;
                        }
                        if !b.am_choking {
                            b_score += 1;
                        }

                        // Natural order is Descending (higher score is better).
                        b_score.cmp(&a_score)
                    }
                    PeerSortColumn::Completed => {
                        // Use the torrent's actual piece count as the source of truth.
                        let total_pieces = state.number_of_pieces_total as usize;
                        if total_pieces == 0 {
                            return std::cmp::Ordering::Equal;
                        }

                        // Count how many pieces peer 'a' has, but don't count more than actually exist.
                        let a_completed =
                            a.bitfield.iter().take(total_pieces).filter(|&&h| h).count();
                        let a_percent = a_completed as f64 / total_pieces as f64;

                        let b_completed =
                            b.bitfield.iter().take(total_pieces).filter(|&&h| h).count();
                        let b_percent = b_completed as f64 / total_pieces as f64;

                        b_percent.total_cmp(&a_percent)
                    }
                    PeerSortColumn::Address => a.address.cmp(&b.address),
                    PeerSortColumn::Client => a.peer_id.cmp(&b.peer_id),
                    PeerSortColumn::Action => a.last_action.cmp(&b.last_action),
                    PeerSortColumn::DL => a.download_speed_bps.cmp(&b.download_speed_bps),
                    PeerSortColumn::UL => a.upload_speed_bps.cmp(&b.upload_speed_bps),
                    PeerSortColumn::TotalDL => a.total_downloaded.cmp(&b.total_downloaded),
                    PeerSortColumn::TotalUL => a.total_uploaded.cmp(&b.total_uploaded),
                };

                // This block now correctly applies the final direction
                if sort_direction == SortDirection::Ascending {
                    ordering
                } else {
                    ordering.reverse()
                }
            });

            // UPDATE: Change the headers for the new columns.
            let peer_header_cells = PEER_HEADERS.iter().enumerate().map(|(i, h)| {
                let is_selected = app_state.selected_header == SelectedHeader::Peer(i);
                let (sort_col, sort_dir) = app_state.peer_sort;
                let is_sorting_by_this = sort_col == *h;
                let mut style = Style::default().fg(theme::YELLOW);
                let text = match h {
                    PeerSortColumn::Flags => "Flags",
                    PeerSortColumn::Address => "Address",
                    PeerSortColumn::Client => "Client",
                    PeerSortColumn::Action => "Action",
                    PeerSortColumn::Completed => "Done %",
                    PeerSortColumn::DL => "DL Speed",
                    PeerSortColumn::UL => "UL Speed",
                    PeerSortColumn::TotalDL => "Total DL",
                    PeerSortColumn::TotalUL => "Total UL",
                };

                let mut text_with_indicator = text.to_string();
                if is_sorting_by_this {
                    style = style.fg(theme::MAUVE);
                    let indicator = if sort_dir == SortDirection::Ascending {
                        " ▲"
                    } else {
                        " ▼"
                    };
                    text_with_indicator.push_str(indicator);
                }
                let mut text_span = Span::styled(text, style);
                if is_selected {
                    text_span = text_span.underlined().bold();
                }
                let mut spans = vec![text_span];
                if is_sorting_by_this {
                    let indicator = if sort_dir == SortDirection::Ascending {
                        " ▲"
                    } else {
                        " ▼"
                    };
                    spans.push(Span::styled(indicator, style));
                }
                Cell::from(Line::from(spans))
            });
            let peer_header = Row::new(peer_header_cells).height(1);

            // UPDATE: Iterate over the new `sorted_peers` vector and use the new fields.
            let peer_rows = sorted_peers.iter().map(|peer| {
                let row_color = if peer.download_speed_bps == 0 && peer.upload_speed_bps == 0 {
                    theme::SURFACE1
                } else {
                    ip_to_color(&peer.address)
                };

                let flags_spans = Line::from(vec![
                    // 1. You are interested (I want pieces) - Toned-down BLUE
                    Span::styled(
                        "■",
                        Style::default().fg(if peer.am_interested {
                            theme::SAPPHIRE // NEW: Deeper Blue
                        } else {
                            theme::SURFACE1
                        }),
                    ),
                    // 2. They are choking me (I can't download) - Toned-down RED
                    Span::styled(
                        "■",
                        Style::default().fg(if peer.peer_choking {
                            theme::MAROON // NEW: Deeper Red/Maroon
                        } else {
                            theme::SURFACE1
                        }),
                    ),
                    // 3. They are interested in me (They want pieces) - Toned-down GREEN
                    Span::styled(
                        "■",
                        Style::default().fg(if peer.peer_interested {
                            theme::TEAL // NEW: Softer Green/Teal
                        } else {
                            theme::SURFACE1
                        }),
                    ),
                    // 4. I am choking them (I am not uploading) - Toned-down YELLOW
                    Span::styled(
                        "■",
                        Style::default().fg(if peer.am_choking {
                            theme::PEACH // NEW: Muted Yellow/Peach
                        } else {
                            theme::SURFACE1
                        }),
                    ),
                ]);

                let total_pieces_from_torrent = state.number_of_pieces_total as usize;
                let percentage = if total_pieces_from_torrent > 0 {
                    // Count how many pieces the peer has, but cap the iteration at the actual number of pieces.
                    let completed_pieces = peer
                        .bitfield
                        .iter()
                        .take(total_pieces_from_torrent)
                        .filter(|&&have| have)
                        .count();

                    // If the peer has every piece, they are a seeder (100%).
                    if completed_pieces == total_pieces_from_torrent {
                        100.0
                    } else {
                        (completed_pieces as f64 / total_pieces_from_torrent as f64) * 100.0
                    }
                } else {
                    0.0 // Default to 0.0 if torrent metadata isn't fully loaded yet.
                };
                Row::new(vec![
                    Cell::from(flags_spans),
                    Cell::from(peer.address.clone()),
                    Cell::from(parse_peer_id(&peer.peer_id)),
                    Cell::from(peer.last_action.clone()),
                    Cell::from(format!("{:.1}%", percentage)),
                    Cell::from(format_speed(peer.download_speed_bps)),
                    Cell::from(format_speed(peer.upload_speed_bps)),
                    Cell::from(format_bytes(peer.total_downloaded)),
                    Cell::from(format_bytes(peer.total_uploaded)),
                ])
                .style(Style::default().fg(row_color))
            });

            let peer_widths = [
                Constraint::Length(5),      // Flags
                Constraint::Percentage(20), // Address
                Constraint::Percentage(15), // Client <-- ADD
                Constraint::Percentage(20), // Last Action
                Constraint::Percentage(5),  // Done
                Constraint::Percentage(10), // DL Speed
                Constraint::Percentage(10), // UL Speed
                Constraint::Percentage(10), // Total DL
                Constraint::Percentage(5),  // Total UL
            ];

            let peer_border_style = if matches!(app_state.selected_header, SelectedHeader::Peer(_))
            {
                Style::default().fg(theme::MAUVE) // Active color
            } else {
                Style::default().fg(theme::SURFACE2) // Inactive color
            };

            let title_width = peers_chunk.width.saturating_sub(4) as usize; // Account for borders and padding
            let truncated_name = if app_state.anonymize_torrent_names {
                format!("Peers for Torrent {}", app_state.selected_torrent_index + 1)
            } else {
                truncate_with_ellipsis(&state.torrent_name, title_width)
            };
            let download_path_str = torrent.latest_state.download_path.to_string_lossy();
            let footer_width = peers_chunk.width.saturating_sub(2) as usize; // Account for borders
            let truncated_path = if app_state.anonymize_torrent_names {
                String::from("/download/path/for/torrents")
            } else {
                truncate_with_ellipsis(&download_path_str, footer_width)
            };


            let peers_table = Table::new(peer_rows, peer_widths)
                .header(peer_header)
                .block(
                    Block::default()
                        .title(Span::styled(
                            truncated_name,
                            Style::default().fg(theme::SKY),
                        ))
                        .borders(Borders::ALL)
                        .border_style(peer_border_style)
                        .title_bottom(Span::styled(truncated_path, Style::default().fg(theme::SUBTEXT0))),
                );

            // Render the new table in its dedicated chunk
            f.render_widget(peers_table, peers_chunk);

            let dl_history = &torrent.download_history;
            let ul_history = &torrent.upload_history;
            const ACTIVITY_WINDOW: usize = 60;
            let check_dl_slice = &dl_history[dl_history.len().saturating_sub(ACTIVITY_WINDOW)..];
            let check_ul_slice = &ul_history[ul_history.len().saturating_sub(ACTIVITY_WINDOW)..];
            let has_dl_activity = check_dl_slice.iter().any(|&s| s > 0);
            let has_ul_activity = check_ul_slice.iter().any(|&s| s > 0);

            // 2. Conditionally render based on activity to maximize screen real estate.
            if has_dl_activity && !has_ul_activity {
                // --- Case 1: Only Download is active ---
                // Size the data window to the full width of the sparkline area.
                let width = sparkline_chunk.width.saturating_sub(2).max(1) as usize;
                let dl_slice = &dl_history[dl_history.len().saturating_sub(width)..];

                let max_speed = dl_slice.iter().max().copied().unwrap_or(1);
                let nice_max_speed = calculate_nice_upper_bound(max_speed).max(1);

                let dl_sparkline = Sparkline::default()
                    .block(
                        Block::default()
                            .title(Span::styled(
                                format!("DL Activity (Peak: {})", format_speed(nice_max_speed)),
                                Style::default().fg(theme::SUBTEXT0),
                            ))
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(theme::SURFACE2)),
                    )
                    .data(dl_slice)
                    .max(nice_max_speed)
                    .style(Style::default().fg(theme::BLUE));
                f.render_widget(dl_sparkline, sparkline_chunk);
            } else if !has_dl_activity && has_ul_activity {
                // --- Case 2: Only Upload is active ---
                // Size the data window to the full width of the sparkline area.
                let width = sparkline_chunk.width.saturating_sub(2).max(1) as usize;
                let ul_slice = &ul_history[ul_history.len().saturating_sub(width)..];

                let max_speed = ul_slice.iter().max().copied().unwrap_or(1);
                let nice_max_speed = calculate_nice_upper_bound(max_speed).max(1);
                let ul_sparkline = Sparkline::default()
                    .block(
                        Block::default()
                            .title(Span::styled(
                                format!("UL Activity (Peak: {})", format_speed(nice_max_speed)),
                                Style::default().fg(theme::SUBTEXT0),
                            ))
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(theme::SURFACE2)),
                    )
                    .data(ul_slice)
                    .max(nice_max_speed)
                    .style(Style::default().fg(theme::GREEN));
                f.render_widget(ul_sparkline, sparkline_chunk);
            } else {
                // --- Case 3: Both are active, or both are idle ---
                // Show them side-by-side.
                let sparkline_chunks =
                    Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .split(sparkline_chunk);
                let dl_sparkline_chunk = sparkline_chunks[0];
                let ul_sparkline_chunk = sparkline_chunks[1];

                // Dynamically size each sparkline's data window to its respective chunk width.
                let dl_width = dl_sparkline_chunk.width.saturating_sub(2).max(1) as usize;
                let ul_width = ul_sparkline_chunk.width.saturating_sub(2).max(1) as usize;

                let dl_slice = &dl_history[dl_history.len().saturating_sub(dl_width)..];
                let ul_slice = &ul_history[ul_history.len().saturating_sub(ul_width)..];

                let max_dl = dl_slice.iter().max().copied().unwrap_or(0);
                let max_ul = ul_slice.iter().max().copied().unwrap_or(0);

                // Calculate a separate "nice" max for each sparkline
                let dl_nice_max = calculate_nice_upper_bound(max_dl).max(1);
                let ul_nice_max = calculate_nice_upper_bound(max_ul).max(1);

                let dl_sparkline = Sparkline::default()
                    .block(
                        Block::default()
                            .title(Span::styled(
                                format!("DL (Peak: {})", format_speed(dl_nice_max)),
                                Style::default().fg(theme::SUBTEXT0),
                            ))
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(theme::SURFACE2)),
                    )
                    .data(dl_slice)
                    .max(dl_nice_max)
                    .style(Style::default().fg(theme::BLUE));
                f.render_widget(dl_sparkline, dl_sparkline_chunk);

                let ul_sparkline = Sparkline::default()
                    .block(
                        Block::default()
                            .title(Span::styled(
                                format!("UL (Peak: {})", format_speed(ul_nice_max)),
                                Style::default().fg(theme::SUBTEXT0),
                            ))
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(theme::SURFACE2)),
                    )
                    .data(ul_slice)
                    .max(ul_nice_max)
                    .style(Style::default().fg(theme::GREEN));
                f.render_widget(ul_sparkline, ul_sparkline_chunk);
            }
        }
    }
}

fn draw_footer(f: &mut Frame, app_state: &AppState, settings: &Settings, footer_chunk: Rect) {
    let footer_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(20),
            Constraint::Percentage(65),
            Constraint::Percentage(15),
        ])
        .split(footer_chunk);

    let client_id_chunk = footer_layout[0];
    let current_dl_speed = *app_state.avg_download_history.last().unwrap_or(&0);
    let current_ul_speed = *app_state.avg_upload_history.last().unwrap_or(&0);

    #[cfg(all(feature = "dht", feature = "pex"))]
    let client_display_line = Line::from(vec![
        Span::styled(
            "super",
            speed_to_style(current_dl_speed).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "seedr",
            speed_to_style(current_ul_speed).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" v{}", APP_VERSION),
            Style::default().fg(theme::SUBTEXT1),
        ),
    ]);

    #[cfg(not(all(feature = "dht", feature = "pex")))]
    let client_display_line = Line::from(vec![
        Span::styled("super", Style::default().fg(theme::SURFACE2))
            .add_modifier(Modifier::CROSSED_OUT),
        Span::styled("seedr", Style::default().fg(theme::SURFACE2))
            .add_modifier(Modifier::CROSSED_OUT),
        Span::styled(
            " [PRIVATE]",
            Style::default().fg(theme::RED).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" v{}", APP_VERSION),
            Style::default().fg(theme::SUBTEXT1),
        ),
    ]);

    let client_id_paragraph = Paragraph::new(client_display_line)
        .style(Style::default().fg(theme::SUBTEXT1))
        .alignment(Alignment::Left);
    f.render_widget(client_id_paragraph, client_id_chunk);

    let commands_chunk = footer_layout[1];
    let status_chunk = footer_layout[2];

    // --- RENDER FOOTER COMMANDS ---
    let help_key = if app_state.system_warning.is_some() {
        vec![
            Span::styled("[m]", Style::default().fg(theme::TEAL)),
            Span::styled("anual/help (warning!)", Style::default().fg(theme::YELLOW)),
        ]
    } else {
        vec![
            Span::styled("[m]", Style::default().fg(theme::TEAL)),
            Span::raw("anual/help"),
        ]
    };
    let mut footer_spans = Line::from(vec![
        Span::styled("↑↓", Style::default().fg(theme::BLUE)),
        Span::raw(" "),
        Span::styled("←→", Style::default().fg(theme::BLUE)),
        Span::raw(" navigate | "),
        Span::styled("[q]", Style::default().fg(theme::RED)),
        Span::raw("uit | "),
        Span::styled("[v]", Style::default().fg(theme::TEAL)),
        Span::raw("paste | "),
        Span::styled("[p]", Style::default().fg(theme::GREEN)),
        Span::raw("ause/resume | "),
        Span::styled("[d]", Style::default().fg(theme::YELLOW)),
        Span::raw("elete | "),
        Span::styled("[s]", Style::default().fg(theme::MAUVE)),
        Span::raw("ort | "),
        Span::styled("[c]", Style::default().fg(theme::LAVENDER)),
        Span::raw("onfig | "),
        Span::styled("[t]", Style::default().fg(theme::SAPPHIRE)),
        Span::raw("ime | "),
        Span::styled("[z]", Style::default().fg(theme::SUBTEXT0)),
        Span::raw("en | "),
        Span::styled("[x]", Style::default().fg(theme::TEAL)),
        Span::raw("ensor | "),
    ]);
    footer_spans.extend(help_key);

    let footer_keys = footer_spans.alignment(Alignment::Center);
    let footer_paragraph = Paragraph::new(footer_keys).style(Style::default().fg(theme::SUBTEXT1));
    f.render_widget(footer_paragraph, commands_chunk);

    let port_style = if app_state.externally_accessable_port {
        Style::default().fg(theme::GREEN)
    } else {
        Style::default().fg(theme::RED)
    };
    let port_text = if app_state.externally_accessable_port {
        "Open"
    } else {
        "Closed"
    };

    let footer_status = Line::from(vec![
        Span::raw("Port: "),
        Span::styled(settings.client_port.to_string(), port_style),
        Span::raw(" ["),
        Span::styled(port_text, port_style),
        Span::raw("]"),
    ])
    .alignment(Alignment::Right);

    let status_paragraph =
        Paragraph::new(footer_status).style(Style::default().fg(theme::SUBTEXT1));
    f.render_widget(status_paragraph, status_chunk);
}

fn draw_config_screen(
    f: &mut Frame,
    settings: &Settings,
    selected_index: usize,
    items: &[ConfigItem],
    editing: &Option<(ConfigItem, String)>,
) {
    let area = centered_rect(80, 60, f.area());
    f.render_widget(Clear, f.area());

    let block = Block::default()
        .title(Span::styled("Config", Style::default().fg(theme::MAUVE)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::SURFACE2));

    let inner_area = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(2)])
        .split(inner_area);

    let settings_area = chunks[0];
    let footer_area = chunks[1];

    // Create a layout with one row for each setting
    let rows_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints(
            items
                .iter()
                .map(|_| Constraint::Length(1))
                .collect::<Vec<_>>(),
        )
        .split(settings_area);

    for (i, item) in items.iter().enumerate() {
        let (name_str, value_str) = match item {
            ConfigItem::ClientPort => ("Listen Port", settings.client_port.to_string()),
            ConfigItem::DefaultDownloadFolder => (
                "Default Download Folder",
                path_to_string(settings.default_download_folder.as_deref()),
            ),
            ConfigItem::WatchFolder => (
                "Torrent Watch Folder",
                path_to_string(settings.watch_folder.as_deref()),
            ),
            ConfigItem::GlobalDownloadLimit => (
                "Global DL Limit",
                format_limit_bps(settings.global_download_limit_bps),
            ),
            ConfigItem::GlobalUploadLimit => (
                "Global UL Limit",
                format_limit_bps(settings.global_upload_limit_bps),
            ),
        };

        // Create two columns for the name and value
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(rows_layout[i]);

        // Determine if the current row should be highlighted
        let is_highlighted = if let Some((edited_item, _)) = editing {
            *edited_item == *item // Highlight the item being edited
        } else {
            i == selected_index // Highlight the item being navigated
        };

        // --- THIS IS THE LINE YOU WILL CHANGE IN THE NEXT STEP ---
        let row_style = if is_highlighted {
            Style::default().fg(theme::YELLOW) // Bright text for selection
        } else {
            Style::default().fg(theme::TEXT) // Default text color
        };

        // Prepend the selector symbol to the name string
        let name_with_selector = if is_highlighted {
            format!("▶ {}", name_str)
        } else {
            format!("  {}", name_str) // Use spaces to keep alignment
        };

        let name_p = Paragraph::new(name_with_selector).style(row_style);
        f.render_widget(name_p, columns[0]);

        if is_highlighted && editing.is_some() {
            let buffer = &editing.as_ref().unwrap().1;
            // Use the base style, but override the foreground color for the text
            let edit_p = Paragraph::new(buffer.as_str()).style(row_style.fg(theme::YELLOW));
            f.set_cursor_position((columns[1].x + buffer.len() as u16, columns[1].y));
            f.render_widget(edit_p, columns[1]);
        } else {
            let value_p = Paragraph::new(value_str).style(row_style);
            f.render_widget(value_p, columns[1]);
        }
    }

    let help_text = if editing.is_some() {
        Line::from(vec![
            Span::styled("[Enter]", Style::default().fg(theme::GREEN)),
            Span::raw(" to confirm, "),
            Span::styled("[Esc]", Style::default().fg(theme::RED)),
            Span::raw(" to cancel."),
        ])
    } else {
        Line::from(vec![
            Span::raw("Use "),
            Span::styled("↑/↓/k/j", Style::default().fg(theme::YELLOW)),
            Span::raw(" to navigate. "),
            Span::styled("[Enter]", Style::default().fg(theme::YELLOW)),
            Span::raw(" to edit."),
            Span::styled("[Esc]|[q]", Style::default().fg(theme::GREEN)),
            Span::raw(" to Save & Exit, "),
        ])
    };

    let footer_paragraph = Paragraph::new(help_text)
        .alignment(Alignment::Center)
        .style(Style::default().fg(theme::SUBTEXT1));
    f.render_widget(footer_paragraph, footer_area);
}


fn draw_help_popup(f: &mut Frame, app_state: &AppState, mode: &AppMode) {
    let (settings_path_str, log_path_str) = if let Some((config_dir, data_dir)) = get_app_paths() {
        (
            config_dir
                .join("settings.toml")
                .to_string_lossy()
                .to_string(),
            data_dir.join("client.log").to_string_lossy().to_string(),
        )
    } else {
        (
            "Unknown location".to_string(),
            "Unknown location".to_string(),
        )
    };
    // --- END ---

    if let Some(warning_text) = &app_state.system_warning {
        // --- This block handles the WARNING + HELP layout ---
        let area = centered_rect(60, 100, f.area());
        f.render_widget(Clear, area);

        let warning_width = area.width.saturating_sub(2).max(1) as usize;
        let warning_lines = (warning_text.len() as f64 / warning_width as f64).ceil() as u16;
        let warning_block_height = warning_lines.saturating_add(2).max(3);

        let max_warning_height = (area.height as f64 * 0.25).round() as u16;
        let final_warning_height = warning_block_height.min(max_warning_height);

        // --- MODIFIED LAYOUT ---
        // Split into 3 chunks: [Warning, Help, Footer]
        let chunks = Layout::vertical([
            Constraint::Length(final_warning_height), // Use dynamic height
            Constraint::Min(0),                       // The rest for the help table
            Constraint::Length(3), // <-- 2 lines for paths + 1 for border
        ])
        .split(area);

        let warning_paragraph = Paragraph::new(warning_text.as_str())
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::RED)),
            )
            .style(Style::default().fg(theme::YELLOW));
        f.render_widget(warning_paragraph, chunks[0]);

        // The help table now renders in the second chunk.
        draw_help_table(f, mode, chunks[1]); // <-- No scroll passed

        // --- Render the footer in chunks[2] ---
        let footer_block = Block::default()
            .border_style(Style::default().fg(theme::SURFACE2));
        let footer_inner_area = footer_block.inner(chunks[2]);
        f.render_widget(footer_block, chunks[2]);

        let footer_lines = vec![
            Line::from(vec![
                Span::styled("Settings: ", Style::default().fg(theme::TEXT)),
                Span::styled(
                    truncate_with_ellipsis(&settings_path_str, footer_inner_area.width as usize - 10),
                    Style::default().fg(theme::SUBTEXT0),
                ),
            ]),
            Line::from(vec![
                Span::styled("Log File: ", Style::default().fg(theme::TEXT)),
                Span::styled(
                    truncate_with_ellipsis(&log_path_str, footer_inner_area.width as usize - 10),
                    Style::default().fg(theme::SUBTEXT0),
                ),
            ]),
        ];
        let footer_paragraph = Paragraph::new(footer_lines).style(Style::default().fg(theme::TEXT));
        f.render_widget(footer_paragraph, footer_inner_area);
    } else {
        // --- This block handles the NO WARNING + HELP layout ---
        let area = centered_rect(60, 100, f.area());
        f.render_widget(Clear, area); // Clear the whole area first

        // Split into 2 chunks: [Help, Footer]
        let chunks = Layout::vertical([
            Constraint::Min(0),    // Help content
            Constraint::Length(3), // Footer area (2 lines + 1 border)
        ])
        .split(area);

        // Original behavior: just draw the help table centered.
        draw_help_table(f, mode, chunks[0]); // <-- No scroll passed

        // --- Render the footer in chunks[1] ---
        let footer_block = Block::default()
            .border_style(Style::default().fg(theme::SURFACE2));
        let footer_inner_area = footer_block.inner(chunks[1]);
        f.render_widget(footer_block, chunks[1]);

        let footer_lines = vec![
            Line::from(vec![
                Span::styled("Settings: ", Style::default().fg(theme::TEXT)),
                Span::styled(
                    truncate_with_ellipsis(&settings_path_str, footer_inner_area.width as usize - 10),
                    Style::default().fg(theme::SUBTEXT0),
                ),
            ]),
            Line::from(vec![
                Span::styled("Log File: ", Style::default().fg(theme::TEXT)),
                Span::styled(
                    truncate_with_ellipsis(&log_path_str, footer_inner_area.width as usize - 10),
                    Style::default().fg(theme::SUBTEXT0),
                ),
            ]),
        ];
        let footer_paragraph = Paragraph::new(footer_lines).style(Style::default().fg(theme::TEXT));
        f.render_widget(footer_paragraph, footer_inner_area);
    }
}

fn draw_help_table(f: &mut Frame, mode: &AppMode, area: Rect) {
    let (title, rows) = match mode {
        AppMode::Normal | AppMode::Welcome => (
            " Manual / Help ",
            vec![
                Row::new(vec![
                    Cell::from(Span::styled("Ctrl +", Style::default().fg(theme::TEAL))),
                    Cell::from("Zoom in (increase font size)"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled("Ctrl -", Style::default().fg(theme::TEAL))),
                    Cell::from("Zoom out (decrease font size)"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled("q", Style::default().fg(theme::RED))),
                    Cell::from("Quit the application"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled("m", Style::default().fg(theme::MAUVE))),
                    Cell::from("Toggle this help screen"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled("c", Style::default().fg(theme::PEACH))),
                    Cell::from("Open Config screen"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled("z", Style::default().fg(theme::SUBTEXT0))),
                    Cell::from("Toggle Zen/Power Saving mode"),
                ]),
                Row::new(vec![Cell::from(""), Cell::from("")]).height(1),
                // --- List Navigation & Sorting ---
                Row::new(vec![Cell::from(Span::styled(
                    "List Navigation",
                    Style::default().fg(theme::YELLOW),
                ))]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "↑ / ↓ / k / j",
                        Style::default().fg(theme::BLUE),
                    )),
                    Cell::from("Navigate torrents list"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "← / → / h / l",
                        Style::default().fg(theme::BLUE),
                    )),
                    Cell::from("Navigate between header columns"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled("s", Style::default().fg(theme::GREEN))),
                    Cell::from("Change sort order for the selected column"),
                ]),
                Row::new(vec![Cell::from(""), Cell::from("")]).height(1),
                // --- Torrent Management ---
                Row::new(vec![Cell::from(Span::styled(
                    "Torrent Actions",
                    Style::default().fg(theme::YELLOW),
                ))]),
                Row::new(vec![
                    Cell::from(Span::styled("p", Style::default().fg(theme::GREEN))),
                    Cell::from("Pause / Resume selected torrent"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled("d / D", Style::default().fg(theme::RED))),
                    Cell::from("Delete torrent (D includes downloaded files)"),
                ]),
                Row::new(vec![Cell::from(""), Cell::from("")]).height(1),
                // --- Adding Torrents ---
                Row::new(vec![Cell::from(Span::styled(
                    "Adding Torrents",
                    Style::default().fg(theme::YELLOW),
                ))]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "Paste | v",
                        Style::default().fg(theme::SAPPHIRE),
                    )),
                    Cell::from("Paste a magnet link or local file path to add"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled("CLI", Style::default().fg(theme::SAPPHIRE))),
                    Cell::from("Use `superseedr add ...` from another terminal"),
                ]),
                Row::new(vec![Cell::from(""), Cell::from("")]).height(1),
                // --- Graph Controls ---
                Row::new(vec![Cell::from(Span::styled(
                    "Graph & Panes",
                    Style::default().fg(theme::YELLOW),
                ))]),
                Row::new(vec![
                    Cell::from(Span::styled("t / T", Style::default().fg(theme::TEAL))),
                    Cell::from("Switch network graph time scale forward/backward"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled("x", Style::default().fg(theme::TEAL))),
                    Cell::from("Anonymize torrent names"),
                ]),
                Row::new(vec![Cell::from(""), Cell::from("")]).height(1),
                // --- Peer Flags Legend ---
                Row::new(vec![
                    // First Cell (for the first column)
                    Cell::from(Span::styled(
                        "Peer Flags Legend",
                        Style::default().fg(theme::YELLOW),
                    )),
                    // Second Cell (for the second column)
                    Cell::from(Line::from(vec![
                        // Legend pairing: DL/UL status
                        Span::raw("DL: (You "),
                        Span::styled("■", Style::default().fg(theme::SAPPHIRE)), // Toned-Down Interested
                        Span::styled("■", Style::default().fg(theme::MAROON)), // Toned-Down Choked
                        Span::raw(") | UL: (Peer "),
                        Span::styled("■", Style::default().fg(theme::TEAL)), // Toned-Down Interested
                        Span::styled("■", Style::default().fg(theme::PEACH)), // Toned-Down Choking
                        Span::raw(")"),
                    ])),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled("■", Style::default().fg(theme::SAPPHIRE))),
                    Cell::from("You are interested (DL Potential)"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled("■", Style::default().fg(theme::MAROON))),
                    Cell::from("Peer is choking you (DL Block)"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled("■", Style::default().fg(theme::TEAL))),
                    Cell::from("Peer is interested (UL Opportunity)"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled("■", Style::default().fg(theme::PEACH))),
                    Cell::from("You are choking peer (UL Restriction)"),
                ]),
                Row::new(vec![Cell::from(""), Cell::from("")]).height(1),
                Row::new(vec![Cell::from(Span::styled(
                    "Disk Stats Legend",
                    Style::default().fg(theme::YELLOW),
                ))]),
                Row::new(vec![
                    Cell::from(Span::styled("↑ (Read)", Style::default().fg(theme::GREEN))),
                    Cell::from("Data read from disk"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled("↓ (Write)", Style::default().fg(theme::SKY))),
                    Cell::from("Data written to disk"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled("Seek", Style::default().fg(theme::TEXT))),
                    Cell::from("Avg. distance between I/O ops (lower is better)"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled("Latency", Style::default().fg(theme::TEXT))),
                    Cell::from("Time to complete one I/O op (lower is better)"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled("IOPS", Style::default().fg(theme::TEXT))),
                    Cell::from("I/O Operations Per Second (total workload)"),
                ]),
                Row::new(vec![Cell::from(""), Cell::from("")]).height(1),
                Row::new(vec![Cell::from(Span::styled(
                    "Self-Tuning Legend",
                    Style::default().fg(theme::YELLOW),
                ))]),
                Row::new(vec![
                    Cell::from(Span::styled("Best Score", Style::default().fg(theme::TEXT))),
                    Cell::from(
                        "Score measuring if randomized changes resulted in optimial speeds.",
                    ),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "Next seconds",
                        Style::default().fg(theme::TEXT),
                    )),
                    Cell::from("Countdown to try a new random resource adjustment (file handles)"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled("(+/-)", Style::default().fg(theme::TEXT))),
                    Cell::from("Random setting change between resources. (Green=Good, Red=Bad)"),
                ]),
                Row::new(vec![Cell::from(""), Cell::from("")]).height(1),
                Row::new(vec![Cell::from(Span::styled(
                    "Build Features",
                    Style::default().fg(theme::YELLOW),
                ))]),
                Row::new(vec![
                    Cell::from(Span::styled("DHT", Style::default().fg(theme::TEXT))),
                    Cell::from(Line::from(vec![
                        #[cfg(feature = "dht")]
                        Span::styled("ON", Style::default().fg(theme::GREEN)),
                        #[cfg(not(feature = "dht"))]
                        Span::styled(
                            "Not included in this [PRIVATE] build of superseedr.",
                            Style::default().fg(theme::RED),
                        ),
                    ])),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled("Pex", Style::default().fg(theme::TEXT))),
                    Cell::from(Line::from(vec![
                        #[cfg(feature = "pex")]
                        Span::styled("ON", Style::default().fg(theme::GREEN)),
                        #[cfg(not(feature = "pex"))]
                        Span::styled(
                            "Not included in this [PRIVATE] build of superseedr.",
                            Style::default().fg(theme::RED),
                        ),
                    ])),
                ]),
            ],
        ),
        AppMode::Config { .. } => (
            " Help / Config ",
            vec![
                Row::new(vec![
                    Cell::from(Span::styled("Esc / q", Style::default().fg(theme::GREEN))),
                    Cell::from("Save and exit config"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "↑ / ↓ / k / j",
                        Style::default().fg(theme::BLUE),
                    )),
                    Cell::from("Navigate items"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled(
                        "← / → / h / l",
                        Style::default().fg(theme::BLUE),
                    )),
                    Cell::from("Decrease / Increase value"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled("Enter", Style::default().fg(theme::YELLOW))),
                    Cell::from("Start or confirm editing"),
                ]),
            ],
        ),
        AppMode::FilePicker(_) | AppMode::ConfigPathPicker { .. } => (
            " Help / File Browser ",
            vec![
                Row::new(vec![
                    Cell::from(Span::styled("Esc", Style::default().fg(theme::RED))),
                    Cell::from("Cancel selection"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled("Tab", Style::default().fg(theme::GREEN))),
                    Cell::from("Confirm selection"),
                ]),
                Row::new(vec![Cell::from(""), Cell::from("")]).height(1),
                Row::new(vec![
                    Cell::from(Span::styled("↑ / ↓", Style::default().fg(theme::BLUE))),
                    Cell::from("Navigate files"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled("←", Style::default().fg(theme::BLUE))),
                    Cell::from("Go to parent directory"),
                ]),
                Row::new(vec![
                    Cell::from(Span::styled("→ / Enter", Style::default().fg(theme::BLUE))),
                    Cell::from("Enter directory"),
                ]),
            ],
        ),
        _ => (
            " Help ",
            vec![Row::new(vec![Cell::from(
                "No help available for this view.",
            )])],
        ),
    };

    let help_table = Table::new(rows, [Constraint::Length(20), Constraint::Min(30)]).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::SURFACE2)),
    );

    f.render_widget(Clear, area);
    f.render_widget(help_table, area); // <-- Renders the Table
}

pub fn draw_shutdown_screen(f: &mut Frame, app_state: &AppState) {
    const POPUP_WIDTH: u16 = 40;
    const POPUP_HEIGHT: u16 = 3;

    let area = f.area();
    let width = POPUP_WIDTH.min(area.width);
    let height = POPUP_HEIGHT.min(area.height);

    let vertical_chunks = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(height),
        Constraint::Min(0),
    ])
    .split(area);

    let area = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(width),
        Constraint::Min(0),
    ])
    .split(vertical_chunks[1])[1];

    f.render_widget(Clear, area);

    let container_block = Block::default()
        .title(Span::styled(" Exiting ", Style::default().fg(theme::PEACH)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::SURFACE2));

    let inner_area = container_block.inner(area);

    f.render_widget(container_block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1)])
        .split(inner_area);

    let progress_label = format!("{:.0}%", (app_state.shutdown_progress * 100.0).min(100.0));
    let progress_bar = Gauge::default()
        .ratio(app_state.shutdown_progress)
        .label(progress_label)
        .gauge_style(Style::default().fg(theme::MAUVE).bg(theme::SURFACE0));

    f.render_widget(progress_bar, chunks[0]);
}

fn draw_power_saving_screen(f: &mut Frame, app_state: &AppState, settings: &Settings) {
    const TRANQUIL_MESSAGES: &[&str] = &[
        "Quietly seeding...",
        "Awaiting peers...",
        "Sharing data...",
        "Connecting to the swarm...",
        "Sharing pieces...",
        "The network is vast...",
        "Listening for connections...",
        "Seeding the cloud...",
        "Uptime is a gift...",
        "Data flows...",
        "Maintaining the ratio...",
        "A torrent of tranquility...",
        "A piece at a time...",
        "The swarm is peaceful...",
        "Be the torrent...",
        "Nurturing the swarm...",
        "Awaiting the handshake...",
        "Distributing packets...",
        "The ratio is balanced...",
        "Each piece finds its home...",
        "Announcing to the tracker...",
        "The bitfield is complete...",
    ];

    let dl_speed = *app_state.avg_download_history.last().unwrap_or(&0);
    let ul_speed = *app_state.avg_upload_history.last().unwrap_or(&0);
    let dl_limit = settings.global_download_limit_bps;
    let ul_limit = settings.global_upload_limit_bps;

    // Define the main area for the pop-up
    let area = centered_rect(40, 60, f.area());
    f.render_widget(Clear, area); // Clear the background

    // Define the outer block and get the inner area for our layout
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::SURFACE1));
    let inner_area = block.inner(area);
    f.render_widget(block, area);

    // Create a vertical layout for perfect centering
    let vertical_chunks = Layout::vertical([
        Constraint::Min(0),    // Top spacer
        Constraint::Length(8), // Main content area
        Constraint::Min(0),    // Bottom spacer
        Constraint::Length(1), // Footer command area
    ])
    .split(inner_area);

    let content_area = vertical_chunks[1];
    let footer_area = vertical_chunks[3];

    // --- Prepare Download & Upload Spans ---
    let mut dl_spans = vec![
        Span::styled("DL: ", Style::default().fg(theme::SKY)),
        // --- CORRECTED THIS LINE ---
        Span::styled(format_speed(dl_speed), Style::default().fg(theme::SKY)),
        Span::raw(" / "),
    ];
    if dl_limit > 0 && dl_speed >= dl_limit {
        dl_spans.push(Span::styled(
            format_limit_bps(dl_limit),
            Style::default().fg(theme::RED),
        ));
    } else {
        dl_spans.push(Span::styled(
            format_limit_bps(dl_limit),
            Style::default().fg(theme::SUBTEXT0),
        ));
    }

    let mut ul_spans = vec![
        Span::styled("UL: ", Style::default().fg(theme::TEAL)),
        // --- CORRECTED THIS LINE ---
        Span::styled(format_speed(ul_speed), Style::default().fg(theme::TEAL)),
        Span::raw(" / "),
    ];
    if ul_limit > 0 && ul_speed >= ul_limit {
        ul_spans.push(Span::styled(
            format_limit_bps(ul_limit),
            Style::default().fg(theme::RED),
        ));
    } else {
        ul_spans.push(Span::styled(
            format_limit_bps(ul_limit),
            Style::default().fg(theme::SUBTEXT0),
        ));
    }

    const MESSAGE_INTERVAL_SECONDS: u64 = 500;
    let seconds_since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let seed = seconds_since_epoch / MESSAGE_INTERVAL_SECONDS;
    let mut rng = StdRng::seed_from_u64(seed);
    let message_index = rng.random_range(0..TRANQUIL_MESSAGES.len());
    let current_message = TRANQUIL_MESSAGES[message_index];

    // --- Prepare Main Content Paragraph ---
    let main_content_lines = vec![
        Line::from(vec![
            Span::styled("super", Style::default().fg(theme::SKY)),
            Span::styled("seedr", Style::default().fg(theme::TEAL)),
        ]),
        Line::from(""), // Padding
        Line::from(Span::styled(
            current_message,
            Style::default().fg(theme::SUBTEXT1),
        )),
        Line::from(""), // Padding
        Line::from(dl_spans),
        Line::from(ul_spans),
    ];
    let main_paragraph = Paragraph::new(main_content_lines).alignment(Alignment::Center);

    // --- Prepare Footer Paragraph ---
    let footer_line = Line::from(Span::styled(
        "Press [z] to resume",
        Style::default().fg(theme::SUBTEXT0),
    ));
    let footer_paragraph = Paragraph::new(footer_line).alignment(Alignment::Center);

    // --- Render the paragraphs in their designated layout chunks ---
    f.render_widget(main_paragraph, content_area);
    f.render_widget(footer_paragraph, footer_area);
}

fn draw_status_error_popup(f: &mut Frame, error_text: &str) {
    let popup_width_percent: u16 = 50;
    // We have 6 lines of text, plus 2 for the top/bottom borders.
    let popup_height: u16 = 8;

    // Create a vertical layout to center the popup
    let vertical_chunks = Layout::vertical([
        Constraint::Min(0), // Top spacer
        Constraint::Length(popup_height),
        Constraint::Min(0), // Bottom spacer
    ])
    .split(f.area());

    // Create a horizontal layout to center the popup
    let area = Layout::horizontal([
        Constraint::Percentage((100 - popup_width_percent) / 2),
        Constraint::Percentage(popup_width_percent),
        Constraint::Percentage((100 - popup_width_percent) / 2),
    ])
    .split(vertical_chunks[1])[1]; // Use the middle chunk from the vertical layout

    f.render_widget(Clear, area); // Clear the area behind the popup

    // Create the text for the popup
    let text = vec![
        Line::from(Span::styled(
            "Error",
            Style::default().fg(theme::RED).bold(),
        )),
        Line::from(""),
        Line::from(Span::styled(error_text, Style::default().fg(theme::YELLOW))),
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(
            "[Press Esc to dismiss]",
            Style::default().fg(theme::SUBTEXT1),
        )),
    ];

    // Create the block
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::RED)); // Red border for warning

    // Create the paragraph and render it
    let paragraph = Paragraph::new(text)
        .block(block)
        .alignment(Alignment::Center)
        // This makes sure that if the error message is too long,
        // it just gets cut off instead of wrapping and breaking the box height.
        .wrap(Wrap { trim: true });

    f.render_widget(paragraph, area);
}

fn draw_welcome_screen(f: &mut Frame) {
    let text = vec![
        Line::from(Span::styled(
            "A BitTorrent Client in your Terminal",
            Style::default(),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "How to Get Started:",
            Style::default().fg(theme::YELLOW).bold(),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(" 1. ", Style::default().fg(theme::GREEN)),
            Span::raw("Paste (Ctrl+V) a "),
            Span::styled("magnet link", Style::default().fg(theme::PEACH)),
            Span::raw(" or "),
            Span::styled("`.torrent` file path", Style::default().fg(theme::PEACH)),
            Span::raw("."),
        ]),
        Line::from("    A file picker will appear to choose a download location for magnet links."),
        Line::from(""),
        Line::from(vec![
            Span::styled(" 2. ", Style::default().fg(theme::GREEN)),
            Span::raw("Use the CLI in another terminal while this TUI is running:"),
        ]),
        Line::from(Span::styled(
            "   $ superseedr \"magnet:?xt=urn:btih:...\"",
            Style::default().fg(theme::SURFACE2),
        )),
        Line::from(Span::styled(
            "   $ superseedr \"/path/to/my.torrent\"",
            Style::default().fg(theme::SURFACE2),
        )),
        Line::from(vec![
            Span::raw("    Note: CLI requires a default download path. Press "),
            Span::styled("[c]", Style::default().fg(theme::MAUVE)),
            Span::raw(" to configure."),
        ]),
        Line::from(""),
        Line::from(""),
        Line::from(vec![
            Span::styled(" [m] ", Style::default().fg(theme::TEAL)),
            Span::styled("for manual/help", Style::default().fg(theme::SUBTEXT1)),
            Span::styled(" | ", Style::default().fg(theme::SURFACE2)),
            Span::styled("[Esc] ", Style::default().fg(theme::RED)),
            Span::styled("to dismiss", Style::default().fg(theme::SUBTEXT1)),
        ]),
    ];

    // --- LAYOUT LOGIC ---

    // 1. Calculate content dimensions
    let text_height = text.len() as u16;
    let text_width = text.iter().map(|line| line.width()).max().unwrap_or(0) as u16;

    // 2. Define padding *inside* the box
    let horizontal_padding: u16 = 4; // 2 chars on each side
    let vertical_padding: u16 = 2; // 1 row top/bottom

    // 3. Calculate the total box dimensions, adding +2 for the borders
    let box_width = (text_width + horizontal_padding + 2).min(f.area().width);
    let box_height = (text_height + vertical_padding + 2).min(f.area().height);

    // 4. Create a centered rect for the box
    let vertical_chunks = Layout::vertical([
        Constraint::Min(0), // Top spacer
        Constraint::Length(box_height),
        Constraint::Min(0), // Bottom spacer
    ])
    .split(f.area()); // Split the whole frame area

    let area = Layout::horizontal([
        Constraint::Min(0), // Left spacer
        Constraint::Length(box_width),
        Constraint::Min(0), // Right spacer
    ])
    .split(vertical_chunks[1])[1]; // Get the middle-middle chunk

    // 5. Render the box and content
    f.render_widget(Clear, area); // Clear just this new, smaller area

    let block = Block::default()
        .title(Span::styled(
            " Welcome to superseedr! ",
            Style::default().fg(theme::MAUVE),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::SURFACE2));

    let inner_area = block.inner(area); // Get inner area of our new box
    f.render_widget(block, area); // Render the box

    // 6. Center the text within the new box's inner_area
    let vertical_chunks_inner = Layout::vertical([
        Constraint::Min(0), // Top spacer
        Constraint::Length(text_height),
        Constraint::Min(0), // Bottom spacer
    ])
    .split(inner_area);

    let horizontal_chunks_inner = Layout::horizontal([
        Constraint::Min(0), // Left spacer
        Constraint::Length(text_width),
        Constraint::Min(0), // Right spacer
    ])
    .split(vertical_chunks_inner[1]);

    let paragraph = Paragraph::new(text)
        .style(Style::default().fg(theme::TEXT))
        .alignment(Alignment::Left);

    f.render_widget(paragraph, horizontal_chunks_inner[1]);
}


