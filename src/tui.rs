// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use ratatui::symbols::Marker;
use ratatui::{prelude::*, symbols, widgets::*};

use crate::tui_formatters::*;

use crate::app::GraphDisplayMode;
use crate::app::PeerInfo;

use crate::app::{
    AppMode, AppState, ConfigItem, SelectedHeader, TorrentControlState, PEER_HEADERS,
    TORRENT_HEADERS,
};

use throbber_widgets_tui::Throbber;

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
        AppMode::DeleteConfirm { .. } => {
            draw_delete_confirm_dialog(f, app_state);
            return;
        }
        AppMode::DownloadPathPicker(file_explorer) => {
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
                Span::styled("[Tab]", Style::default().fg(theme::GREEN)), // Use Enter
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

    let left_pane = top_chunks[0]; // The entire left pane
    let right_pane = top_chunks[1]; // The entire right pane

    // --- Right Pane Layout ---
    let right_pane_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(9), // Top area
            Constraint::Min(0),    // Bottom area (Peers table)
        ])
        .split(right_pane);

    let details_area_chunk = right_pane_chunks[0]; // The whole top-right area
    let peers_chunk = right_pane_chunks[1]; // Bottom-right

    // Split the top-right area to make room for the chart
    let details_chunks = Layout::horizontal([
        Constraint::Percentage(20), // Left side for Details text
        Constraint::Percentage(80), // Right side for Global Peer Chart
    ])
    .split(details_area_chunk);

    let details_text_chunk = details_chunks[0]; // Top-right-left
    let peer_chart_chunk = details_chunks[1]; // Top-right-right (NEW)
                                              // --- End Right Pane Layout ---

    // draw_left_pane handles its own internal layout now
    draw_left_pane(f, app_state, left_pane);

    // Pass the new, smaller text chunk
    draw_right_pane(f, app_state, details_text_chunk, peers_chunk);

    draw_network_chart(f, app_state, chart_chunk);

    draw_stats_panel(f, app_state, settings, stats_chunk);

    draw_peer_history_sparklines(f, app_state, peer_chart_chunk);

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
    let left_pane_chunks = Layout::vertical([
        Constraint::Min(0),    // Torrent list
        Constraint::Length(5), // Torrent UL/DL Sparklines
    ])
    .split(left_pane); // Split the incoming area

    let torrent_list_chunk = left_pane_chunks[0];
    let torrent_sparkline_chunk = left_pane_chunks[1];

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
    let table_inner_area = table_block.inner(torrent_list_chunk);
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

    let rows =
        app_state
            .torrent_list_order
            .iter()
            .enumerate()
            .map(|(i, info_hash)| match app_state.torrents.get(info_hash) {
                Some(torrent) => {
                    let state = &torrent.latest_state;
                    let progress = if state.number_of_pieces_total > 0 {
                        (state.number_of_pieces_completed as f64
                            / state.number_of_pieces_total as f64)
                            * 100.0
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
                        row_cells.insert(
                            0,
                            Cell::from(format!("{:.1}%", progress)).style(progress_style),
                        );
                    }

                    Row::new(row_cells).style(row_style)
                }
                None => Row::new(vec![
                    Cell::from(""),
                    Cell::from("Missing torrent data..."),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(""),
                ]),
            });

    let border_style = if matches!(app_state.selected_header, SelectedHeader::Torrent(_)) {
        Style::default().fg(theme::MAUVE) // Active color
    } else {
        Style::default().fg(theme::SURFACE2) // Inactive color
    };

    let mut title_spans = Vec::new();
    if app_state.is_searching {
        title_spans.push(Span::raw("Search: /"));
        title_spans.push(Span::styled(
            app_state.search_query.clone(),
            Style::default().fg(theme::YELLOW),
        ));
        title_spans.push(Span::raw(" "));
    } else if !app_state.search_query.is_empty() {
        title_spans.push(Span::styled("Torrents ", Style::default().fg(theme::GREEN)));
        title_spans.push(Span::styled("[", Style::default().fg(theme::SUBTEXT1)));
        title_spans.push(Span::styled(
            app_state.search_query.clone(),
            Style::default()
                .fg(theme::SUBTEXT1)
                .add_modifier(Modifier::ITALIC),
        ));
        title_spans.push(Span::styled("]", Style::default().fg(theme::SUBTEXT1)));
    } else {
        title_spans.push(Span::styled("Torrents", Style::default().fg(theme::GREEN)));
    }

    if let Some(info_hash) = app_state
        .torrent_list_order
        .get(app_state.selected_torrent_index)
    {
        if let Some(torrent) = app_state.torrents.get(info_hash) {
            let path_to_display = if app_state.anonymize_torrent_names {
                "/download/path/for/torrents".to_string()
            } else {
                torrent
                    .latest_state
                    .download_path
                    .to_string_lossy()
                    .to_string()
            };

            let current_title_len: usize = title_spans.iter().map(|s| s.width()).sum();
            let available_width = torrent_list_chunk
                .width
                .saturating_sub(current_title_len as u16)
                .saturating_sub(5);

            let truncated_path = truncate_with_ellipsis(&path_to_display, available_width as usize);

            title_spans.push(Span::raw(" "));
            title_spans.push(Span::styled(
                truncated_path,
                Style::default().fg(theme::SUBTEXT0),
            ));
        }
    }

    let title_content = Line::from(title_spans);

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title_content);

    if let Some(info_hash) = app_state
        .torrent_list_order
        .get(app_state.selected_torrent_index)
    {
        if let Some(torrent) = app_state.torrents.get(info_hash) {
            let state = &torrent.latest_state;
            let footer_width = torrent_list_chunk.width.saturating_sub(4) as usize;

            let name_to_display = if app_state.anonymize_torrent_names {
                format!("Torrent {}", app_state.selected_torrent_index + 1)
            } else {
                state.torrent_name.clone()
            };

            let truncated_name = truncate_with_ellipsis(&name_to_display, footer_width);

            block = block.title_bottom(Span::styled(
                truncated_name,
                Style::default().fg(theme::YELLOW),
            ));
        }
    }

    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .row_highlight_style(Style::default().add_modifier(Modifier::BOLD));

    if app_state.is_searching {
        f.set_cursor_position(Position {
            x: torrent_list_chunk.x + 10 + app_state.search_query.len() as u16,
            y: torrent_list_chunk.y,
        });
    }

    f.render_stateful_widget(table, torrent_list_chunk, &mut table_state);
    draw_torrent_sparklines(f, app_state, torrent_sparkline_chunk);
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

    let (
        dl_history_source,
        ul_history_source,
        backoff_history_source_ms,
        time_window_points,
        _time_unit_secs,
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

    let dl_len = dl_history_source.len();
    let ul_len = ul_history_source.len();
    let backoff_len = backoff_history_source_ms.len();

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

    let stable_max_speed = dl_history_slice // <- Use the slice
        .iter()
        .chain(ul_history_slice.iter()) // <- Use the slice
        .max()
        .copied()
        .unwrap_or(10_000); // Default to 10 Kbps if no data
    let nice_max_speed = calculate_nice_upper_bound(stable_max_speed);

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
                .bounds([0.0, nice_max_speed as f64]) // Now using the correct max
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

    let thrash_text: String;
    let thrash_style: Style;

    let baseline_val = app_state.adaptive_max_scpb;

    let thrash_score_val = app_state.global_disk_thrash_score;
    let thrash_score_str = format!("{:.0}", thrash_score_val);

    if thrash_score_val < 0.01 {
        thrash_text = format!("- ({})", thrash_score_str);
        thrash_style = Style::default().fg(theme::SUBTEXT0);
    } else if baseline_val == 0.0 {
        thrash_text = format!("∞ ({})", thrash_score_str);
        thrash_style = Style::default().fg(theme::RED).bold();
    } else {
        let diff = thrash_score_val - baseline_val;
        let thrash_percentage = (diff / baseline_val) * 100.0;

        if thrash_percentage > -0.01 && thrash_percentage < 0.01 {
            thrash_text = format!("0.0% ({})", thrash_score_str);
            thrash_style = Style::default().fg(theme::TEXT);
        } else {
            thrash_text = format!("{:+.1}% ({})", thrash_percentage, thrash_score_str);

            if thrash_percentage > 15.0 {
                thrash_style = Style::default().fg(theme::RED).bold();
            } else if thrash_percentage > 0.0 {
                thrash_style = Style::default().fg(theme::YELLOW);
            } else {
                thrash_style = Style::default().fg(theme::GREEN);
            }
        }
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
        Line::from(""),
        Line::from(vec![
            Span::styled("Next Tuning in: ", Style::default().fg(theme::TEXT)),
            Span::raw(format!("{}s", app_state.tuning_countdown)),
        ]),
        Line::from(vec![
            Span::styled("Disk Thrash: ", Style::default().fg(theme::TEAL)),
            Span::styled(thrash_text, thrash_style),
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

fn draw_right_pane(
    f: &mut Frame,
    app_state: &AppState,
    details_text_chunk: Rect,
    peers_chunk: Rect,
) {
    if let Some(info_hash) = app_state
        .torrent_list_order
        .get(app_state.selected_torrent_index)
    {
        if let Some(torrent) = app_state.torrents.get(info_hash) {
            let state = &torrent.latest_state;

            // --- Details Text Block ---
            let details_block = Block::default()
                .title(Span::styled("Details", Style::default().fg(theme::MAUVE)))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::SURFACE2));
            let details_inner_chunk = details_block.inner(details_text_chunk);
            f.render_widget(details_block, details_text_chunk);


            let detail_rows = Layout::vertical([
                Constraint::Length(1), // Progress Gauge
                Constraint::Length(1), // Status
                Constraint::Length(1), // Peers
                Constraint::Length(1), // Written / Size
                Constraint::Length(1), // Pieces
                Constraint::Length(1), // ETA
                Constraint::Length(1), // Announce
            ])
            .split(details_inner_chunk);

            let progress_chunks = Layout::horizontal([Constraint::Length(11), Constraint::Min(0)])
                .split(detail_rows[0]);

            f.render_widget(Paragraph::new("Progress: "), progress_chunks[0]);

            let (progress_ratio, progress_label_text) = if state.number_of_pieces_total > 0 {
                let ratio =
                    state.number_of_pieces_completed as f64 / state.number_of_pieces_total as f64;
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

            let status_text = if state.activity_message.is_empty() {
                "Waiting..."
            } else {
                state.activity_message.as_str()
            };
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("Status:   ", Style::default().fg(theme::TEXT)),
                    Span::raw(status_text),
                ])),
                detail_rows[1],
            );

            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("Peers:    ", Style::default().fg(theme::TEXT)),
                    Span::raw(state.number_of_successfully_connected_peers.to_string()),
                ])),
                detail_rows[2],
            );

            let written_size_spans =
                if state.number_of_pieces_completed < state.number_of_pieces_total {
                    vec![
                        Span::styled("Written:  ", Style::default().fg(theme::TEXT)),
                        Span::raw(format_bytes(state.bytes_written)),
                        Span::raw(format!(" / {}", format_bytes(state.total_size))),
                    ]
                } else {
                    vec![
                        Span::styled("Size:     ", Style::default().fg(theme::TEXT)),
                        Span::raw(format_bytes(state.total_size)),
                    ]
                };
            f.render_widget(
                Paragraph::new(Line::from(written_size_spans)),
                detail_rows[3],
            );

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




            // 1. Create the Block
            let peer_border_style = if matches!(app_state.selected_header, SelectedHeader::Peer(_))
            {
                Style::default().fg(theme::MAUVE) // Active color
            } else {
                Style::default().fg(theme::SURFACE2) // Inactive color
            };

            let title = if state.peers.is_empty() {
                "Swarm Availability"
            } else {
                "Peers" // Title is just "Peers" even if heatmap is shown below
            };

            let peers_block = Block::default()
                .title(Span::styled(title, Style::default().fg(theme::SKY)))
                .borders(Borders::ALL)
                .border_style(peer_border_style);

            // 2. Get the inner area *after* drawing the block
            let inner_peers_area = peers_block.inner(peers_chunk);
            f.render_widget(peers_block, peers_chunk); // Render the block ONCE

            // 3. Decide what to draw *inside* the block
            if state.peers.is_empty() {
                // --- BEHAVIOR 1: No Peers ---

                // Draw the centered heatmap
                draw_swarm_heatmap(
                    f,
                    &state.peers,
                    state.number_of_pieces_total,
                    inner_peers_area,
                );
            } else {
                // --- BEHAVIOR 2: Peers > 0 ---
                // We *always* draw the peer table.
                // We *might* also draw the heatmap if there's space.

                // --- (Build the peer table data) ---
                let mut sorted_peers = state.peers.clone();
                let (sort_by, sort_direction) = app_state.peer_sort;
                sorted_peers.sort_by(|a, b| {
                    let ordering = match sort_by {
                        PeerSortColumn::Flags => {
                            let mut a_score = 0;
                            if !a.peer_choking {
                                a_score += 2;
                            }
                            if !a.am_choking {
                                a_score += 1;
                            }
                            let mut b_score = 0;
                            if !b.peer_choking {
                                b_score += 2;
                            }
                            if !b.am_choking {
                                b_score += 1;
                            }
                            b_score.cmp(&a_score)
                        }
                        PeerSortColumn::Completed => {
                            let total_pieces = state.number_of_pieces_total as usize;
                            if total_pieces == 0 {
                                return std::cmp::Ordering::Equal;
                            }
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

                    if sort_direction == SortDirection::Ascending {
                        ordering
                    } else {
                        ordering.reverse()
                    }
                });

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

                let peer_rows = sorted_peers.iter().map(|peer| {
                    let row_color = if peer.download_speed_bps == 0 && peer.upload_speed_bps == 0 {
                        theme::SURFACE1
                    } else {
                        ip_to_color(&peer.address)
                    };

                    let flags_spans = Line::from(vec![
                        Span::styled(
                            "■",
                            Style::default().fg(if peer.am_interested {
                                theme::SAPPHIRE
                            } else {
                                theme::SURFACE1
                            }),
                        ),
                        Span::styled(
                            "■",
                            Style::default().fg(if peer.peer_choking {
                                theme::MAROON
                            } else {
                                theme::SURFACE1
                            }),
                        ),
                        Span::styled(
                            "■",
                            Style::default().fg(if peer.peer_interested {
                                theme::TEAL
                            } else {
                                theme::SURFACE1
                            }),
                        ),
                        Span::styled(
                            "■",
                            Style::default().fg(if peer.am_choking {
                                theme::PEACH
                            } else {
                                theme::SURFACE1
                            }),
                        ),
                    ]);

                    let total_pieces_from_torrent = state.number_of_pieces_total as usize;
                    let percentage = if total_pieces_from_torrent > 0 {
                        let completed_pieces = peer
                            .bitfield
                            .iter()
                            .take(total_pieces_from_torrent)
                            .filter(|&&have| have)
                            .count();
                        if completed_pieces == total_pieces_from_torrent {
                            100.0
                        } else {
                            (completed_pieces as f64 / total_pieces_from_torrent as f64) * 100.0
                        }
                    } else {
                        0.0
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
                    Constraint::Length(5),
                    Constraint::Percentage(20),
                    Constraint::Percentage(15),
                    Constraint::Percentage(20),
                    Constraint::Percentage(5),
                    Constraint::Percentage(10),
                    Constraint::Percentage(10),
                    Constraint::Percentage(10),
                    Constraint::Percentage(5),
                ];

                let peers_table = Table::new(peer_rows, peer_widths)
                    .header(peer_header)
                    .block(Block::default()); // NO block, it lives in the parent



                // --- NEW LAYOUT LOGIC ---
                let table_height_needed: u16 = 1 + sorted_peers.len() as u16;
                let available_height = inner_peers_area.height;
                let remaining_height = available_height.saturating_sub(table_height_needed);

                const MIN_HEATMAP_HEIGHT: u16 = 4; // Need at least 4 rows to be useful

                if remaining_height >= MIN_HEATMAP_HEIGHT {
                    // --- BEHAVIOR 2a: Draw Table + Heatmap ---
                    let layout_chunks = Layout::vertical([
                        Constraint::Length(table_height_needed), // Exact size for table
                        Constraint::Min(0),                      // Rest for heatmap
                    ])
                    .split(inner_peers_area);

                    let table_area = layout_chunks[0];
                    let heatmap_area = layout_chunks[1];

                    // Render the table in the top chunk
                    f.render_widget(peers_table, table_area);

                    // Render the heatmap in the bottom chunk
                    // We add a small block with a top border to separate it
                    let heatmap_block = Block::default()
                        .title(Span::styled(
                            "Swarm Availability",
                            Style::default().fg(theme::SUBTEXT1),
                        ))
                        .borders(Borders::TOP) // Just a top border
                        .border_style(Style::default().fg(theme::SURFACE2));

                    let heatmap_inner_area = heatmap_block.inner(heatmap_area);
                    f.render_widget(heatmap_block, heatmap_area);
                    draw_swarm_heatmap(
                        f,
                        &state.peers,
                        state.number_of_pieces_total,
                        heatmap_inner_area,
                    );
                } else {
                    // --- BEHAVIOR 2b: Draw Table ONLY ---
                    // Not enough space for heatmap, just render the table in the full area
                    f.render_widget(peers_table, inner_peers_area);
                }
            }
        }
    }
}

fn draw_footer(f: &mut Frame, app_state: &AppState, settings: &Settings, footer_chunk: Rect) {
    let footer_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(60),
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
        Span::styled(" | ", Style::default().fg(theme::SURFACE2)),
        Span::styled(
            app_state.data_rate.to_string(),
            Style::default().fg(theme::YELLOW).bold(),
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
            Span::styled("anual/help (warning)", Style::default().fg(theme::YELLOW)),
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
        Span::styled("[/]", Style::default().fg(theme::YELLOW)),
        Span::raw("search | "),
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

        if let Some((_edited_item, buffer)) = editing {
            if is_highlighted {
                let edit_p = Paragraph::new(buffer.as_str()).style(row_style.fg(theme::YELLOW));
                f.set_cursor_position((columns[1].x + buffer.len() as u16, columns[1].y));
                f.render_widget(edit_p, columns[1]);
            } else {
                let value_p = Paragraph::new(value_str).style(row_style);
                f.render_widget(value_p, columns[1]);
            }
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
            Span::raw(" to edit. "),
            Span::styled("[r]", Style::default().fg(theme::YELLOW)),
            Span::raw("eset to default. "),
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
            data_dir
                .join("logs")
                .join("app.log")
                .to_string_lossy()
                .to_string(),
        )
    } else {
        (
            "Unknown location".to_string(),
            "Unknown location".to_string(),
        )
    };

    if let Some(warning_text) = &app_state.system_warning {
        let area = centered_rect(60, 100, f.area());
        f.render_widget(Clear, area);

        let warning_width = area.width.saturating_sub(2).max(1) as usize;
        let warning_lines = (warning_text.len() as f64 / warning_width as f64).ceil() as u16;
        let warning_block_height = warning_lines.saturating_add(2).max(3);

        let max_warning_height = (area.height as f64 * 0.25).round() as u16;
        let final_warning_height = warning_block_height.min(max_warning_height);


        // Split into 3 chunks: [Warning, Help, Footer]
        let chunks = Layout::vertical([
            Constraint::Length(final_warning_height), // Use dynamic height
            Constraint::Min(0),                       // The rest for the help table
            Constraint::Length(3),                    // <-- 2 lines for paths + 1 for border
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
        let footer_block = Block::default().border_style(Style::default().fg(theme::SURFACE2));
        let footer_inner_area = footer_block.inner(chunks[2]);
        f.render_widget(footer_block, chunks[2]);

        let footer_lines = vec![
            Line::from(vec![
                Span::styled("Settings: ", Style::default().fg(theme::TEXT)),
                Span::styled(
                    truncate_with_ellipsis(
                        &settings_path_str,
                        footer_inner_area.width as usize - 10,
                    ),
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
        let footer_block = Block::default().border_style(Style::default().fg(theme::SURFACE2));
        let footer_inner_area = footer_block.inner(chunks[1]);
        f.render_widget(footer_block, chunks[1]);

        let footer_lines = vec![
            Line::from(vec![
                Span::styled("Settings: ", Style::default().fg(theme::TEXT)),
                Span::styled(
                    truncate_with_ellipsis(
                        &settings_path_str,
                        footer_inner_area.width as usize - 10,
                    ),
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
                    Cell::from(Span::styled("[ / ]", Style::default().fg(theme::TEAL))),
                    Cell::from("Change UI refresh rate (FPS)"),
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
        AppMode::ConfigPathPicker { .. } | AppMode::DownloadPathPicker { .. } => (
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

fn draw_torrent_sparklines(f: &mut Frame, app_state: &AppState, area: Rect) {
    let torrent = app_state
        .torrent_list_order
        .get(app_state.selected_torrent_index)
        .and_then(|info_hash| app_state.torrents.get(info_hash));

    let Some(torrent) = torrent else {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::SURFACE2));
        f.render_widget(block, area);
        return;
    };

    let dl_history = &torrent.download_history;
    let ul_history = &torrent.upload_history;
    const ACTIVITY_WINDOW: usize = 60;
    let check_dl_slice = &dl_history[dl_history.len().saturating_sub(ACTIVITY_WINDOW)..];
    let check_ul_slice = &ul_history[ul_history.len().saturating_sub(ACTIVITY_WINDOW)..];
    let has_dl_activity = check_dl_slice.iter().any(|&s| s > 0);
    let has_ul_activity = check_ul_slice.iter().any(|&s| s > 0);

    if has_dl_activity && !has_ul_activity {
        let width = area.width.saturating_sub(2).max(1) as usize;
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
        f.render_widget(dl_sparkline, area);
    } else if !has_dl_activity && has_ul_activity {
        let width = area.width.saturating_sub(2).max(1) as usize;
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
        f.render_widget(ul_sparkline, area);
    } else if !has_dl_activity && !has_ul_activity {
        let style = Style::default().fg(theme::MAUVE);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::SURFACE2));

        let inner_area = block.inner(area);

        f.render_widget(block, area);

        let vertical_chunks = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner_area);

        let throbber_width = 23;
        let horizontal_chunks = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(throbber_width),
            Constraint::Min(0),
        ])
        .split(vertical_chunks[1]);

        let inner_chunks = Layout::horizontal([
            Constraint::Length(1),
            Constraint::Length(21),
            Constraint::Length(1),
        ])
        .split(horizontal_chunks[1]);

        let throbber_left_area = inner_chunks[0];
        let label_area = inner_chunks[1];
        let throbber_right_area = inner_chunks[2];

        let label_text = Paragraph::new(" Searching for Peers ")
            .style(style)
            .alignment(Alignment::Center);

        let throbber_style = Style::default()
            .fg(theme::LAVENDER)
            .add_modifier(Modifier::BOLD);
        let throbber_widget = Throbber::default().style(throbber_style);

        f.render_widget(label_text, label_area);

        f.render_stateful_widget(
            throbber_widget.clone(),
            throbber_left_area,
            &mut app_state.throbber_holder.borrow_mut().torrent_sparkline,
        );

        f.render_stateful_widget(
            throbber_widget,
            throbber_right_area,
            &mut app_state.throbber_holder.borrow_mut().torrent_sparkline,
        );
    } else {
        let sparkline_chunks =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(area);
        let dl_sparkline_chunk = sparkline_chunks[0];
        let ul_sparkline_chunk = sparkline_chunks[1];

        let dl_width = dl_sparkline_chunk.width.saturating_sub(2).max(1) as usize;
        let ul_width = ul_sparkline_chunk.width.saturating_sub(2).max(1) as usize;
        let dl_slice = &dl_history[dl_history.len().saturating_sub(dl_width)..];
        let ul_slice = &ul_history[ul_history.len().saturating_sub(ul_width)..];
        let max_dl = dl_slice.iter().max().copied().unwrap_or(0);
        let max_ul = ul_slice.iter().max().copied().unwrap_or(0);
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

fn draw_peer_history_sparklines(f: &mut Frame, app_state: &AppState, area: Rect) {
    let selected_torrent = app_state
        .torrent_list_order
        .get(app_state.selected_torrent_index)
        .and_then(|info_hash| app_state.torrents.get(info_hash));

    let Some(torrent) = selected_torrent else {
        let block = Block::default()
            .title(Span::styled(
                "Peer Stream",
                Style::default().fg(theme::SUBTEXT0),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::SURFACE2));
        f.render_widget(block, area);
        return;
    };

    let width = area.width.saturating_sub(2).max(1) as usize;

    let disc_history = &torrent.peer_discovery_history;
    let conn_history = &torrent.peer_connection_history;
    let disconn_history = &torrent.peer_disconnect_history;

    let disc_slice = &disc_history[disc_history.len().saturating_sub(width)..];
    let conn_slice = &conn_history[conn_history.len().saturating_sub(width)..];
    let disconn_slice = &disconn_history[disconn_history.len().saturating_sub(width)..];

    let discovered_count: u64 = disc_slice.iter().sum();
    let connected_count: u64 = conn_slice.iter().sum();
    let disconnected_count: u64 = disconn_slice.iter().sum();

    let legend_line = Line::from(vec![
        Span::styled("Discovered:", Style::default().fg(theme::LAVENDER)),
        Span::raw(discovered_count.to_string()),
        Span::raw(" "),
        Span::styled("Connected:", Style::default().fg(theme::TEAL)),
        Span::raw(connected_count.to_string()),
        Span::raw(" "),
        Span::styled("Disconnected:", Style::default().fg(theme::MAROON)),
        Span::raw(disconnected_count.to_string()),
        Span::raw(" "),
    ]);

    let disc_data: Vec<(f64, f64)> = disc_slice
        .iter()
        .enumerate()
        .filter_map(|(i, &v)| if v > 0 { Some((i as f64, 3.0)) } else { None })
        .collect();

    let conn_data: Vec<(f64, f64)> = conn_slice
        .iter()
        .enumerate()
        .filter_map(|(i, &v)| if v > 0 { Some((i as f64, 2.0)) } else { None })
        .collect();

    let disconn_data: Vec<(f64, f64)> = disconn_slice
        .iter()
        .enumerate()
        .filter_map(|(i, &v)| if v > 0 { Some((i as f64, 1.0)) } else { None })
        .collect();

    let datasets = vec![
        Dataset::default()
            .data(&disc_data)
            .marker(Marker::Block)
            .graph_type(GraphType::Scatter)
            .style(Style::default().fg(theme::LAVENDER)),
        Dataset::default()
            .data(&conn_data)
            .marker(Marker::Block)
            .graph_type(GraphType::Scatter)
            .style(Style::default().fg(theme::TEAL)),
        Dataset::default()
            .data(&disconn_data)
            .marker(Marker::Block)
            .graph_type(GraphType::Scatter)
            .style(Style::default().fg(theme::MAROON)),
    ];

    let discovery_chart = Chart::new(datasets)
        .block(
            Block::default()
                .title_top(
                    Line::from(Span::styled(
                        "Peer Stream",
                        Style::default().fg(theme::SUBTEXT0),
                    ))
                    .alignment(Alignment::Left),
                )
                .title_top(legend_line.alignment(Alignment::Right))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::SURFACE2)),
        )
        .x_axis(
            Axis::default()
                .style(Style::default().fg(theme::OVERLAY0))
                .bounds([0.0, disc_slice.len().saturating_sub(1) as f64]),
        )
        .y_axis(Axis::default().bounds([0.5, 3.5]));

    f.render_widget(discovery_chart, area);
}

// IN: tui.rs

fn draw_swarm_heatmap(
    f: &mut Frame,
    peers: &[PeerInfo],
    total_pieces: u32,
    area: Rect, // This is the *total available area* (from parent block)
) {

    // This creates a new Rect *inside* the area, respecting the margin.
    let padded_area = area.inner(Margin {
        vertical: 1,   // 1 row top, 1 row bottom
        horizontal: 1, // 1 char left, 1 char right
    });


    let total_pieces_usize = total_pieces as usize;

    // --- Generate availability live from the current peer list ---
    let mut availability: Vec<u32> = vec![0; total_pieces_usize];
    if total_pieces_usize > 0 {
        for peer in peers {
            for (i, has_piece) in peer.bitfield.iter().enumerate().take(total_pieces_usize) {
                if *has_piece {
                    availability[i] += 1;
                }
            }
        }
    }

    if total_pieces_usize == 0 {
        let center_text = Paragraph::new("Waiting for metadata...")
            .style(Style::default().fg(theme::SUBTEXT1))
            .alignment(Alignment::Center);
        // Render the "Waiting" text in the new *padded* area
        f.render_widget(center_text, padded_area);
        return;
    }

    // --- Calculate max_avail ---
    // Find the highest peer count for any single piece.
    // .max(1) prevents division by zero if max_avail is 0.
    let max_avail = availability.iter().max().copied().unwrap_or(1).max(1);
    let max_avail_f64 = max_avail as f64; // Convert once for efficiency


    // --- Resampling "Fill" Logic ---

    let available_width = padded_area.width as usize;
    let available_height = padded_area.height as usize;

    let total_cells = (available_width * available_height) as u64;

    if total_cells == 0 {
        return; // No space to draw
    }

    let mut lines = Vec::with_capacity(available_height);
    let total_pieces_u64 = total_pieces_usize as u64;

    for y in 0..available_height {
        let mut spans = Vec::with_capacity(available_width);
        for x in 0..available_width {
            // Calculate the current cell's index (0 to total_cells - 1)
            let cell_index = (y * available_width + x) as u64;

            // Map this cell index to a piece index
            let piece_index = ((cell_index * total_pieces_u64) / total_cells) as usize;

            // Ensure we don't go out of bounds (shouldn't happen, but safe)
            if piece_index >= total_pieces_usize {
                spans.push(Span::raw(" ")); // Should be unreachable, but good safety
                continue;
            }

            let count = availability[piece_index];


            let (piece_char, color) = if count == 0 {
                ('0', theme::SURFACE1) // Grey: No peers have this
            } else {
                // Normalize count against the max
                let norm_val = count as f64 / max_avail_f64;

                let color = if norm_val <= 0.50 {
                    theme::MAUVE // Purple
                } else {
                    theme::BLUE // Brightest "hot" color
                };
                ('1', color)
            };


            spans.push(Span::styled(
                piece_char.to_string(),
                Style::default().fg(color),
            ));
        }
        lines.push(Line::from(spans));
    }

    let heatmap = Paragraph::new(lines);
    f.render_widget(heatmap, padded_area);

}
