// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::{
    App, AppCommand, AppMode, BrowserPane, ConfigItem, ConfigUiState, FileBrowserMode,
    FileMetadata, FilePriority, TorrentDisplayState, TorrentPreviewPayload,
};
use crate::theme::ThemeContext;
use crate::torrent_manager::ManagerCommand;
use crate::tui::formatters::{centered_rect, format_bytes, truncate_with_ellipsis};
use crate::tui::layout::browser::calculate_file_browser_layout;
use crate::tui::screen_context::ScreenContext;
use crate::tui::tree::{RawNode, TreeAction, TreeFilter, TreeMathHelper, TreeViewState};
use ratatui::crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::prelude::{Alignment, Frame, Line, Modifier, Span, Style, Stylize};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use tokio::sync::mpsc::{self, Sender};

const ASCII_TREE_DIR_ICON: &str = "> ";
const ASCII_TREE_FILE_ICON: &str = "  ";
const ASCII_TREE_ROOT_ICON: &str = "> ";

pub struct DownloadConfirmPayload {
    pub base_path: PathBuf,
    pub container_name_to_use: Option<String>,
    pub file_priorities: HashMap<usize, FilePriority>,
}

pub fn draw(
    f: &mut Frame,
    screen: &ScreenContext<'_>,
    state: &TreeViewState,
    data: &[RawNode<FileMetadata>],
    browser_mode: &FileBrowserMode,
) {
    let app_state = screen.ui;
    let ctx = screen.theme;

    let has_preview_content = has_preview_content(
        browser_mode,
        app_state.pending_torrent_path.is_some(),
        !app_state.pending_torrent_link.is_empty(),
        state.cursor_path.as_ref(),
    );

    let preview_file_path = match browser_mode {
        FileBrowserMode::DownloadLocSelection { .. } => app_state.pending_torrent_path.as_ref(),
        FileBrowserMode::File(_) => state.cursor_path.as_ref(),
        _ => None,
    };

    let focused_pane = focused_pane(browser_mode);
    let max_area = centered_rect(90, 80, f.area());
    f.render_widget(Clear, max_area);

    let area = calculate_area(f.area(), has_preview_content);
    let layout = calculate_file_browser_layout(
        area,
        has_preview_content,
        app_state.ui.file_browser.is_searching,
        &focused_pane,
    );

    let (files_border_style, preview_border_style) =
        if let FileBrowserMode::DownloadLocSelection { focused_pane, .. } = browser_mode {
            match focused_pane {
                BrowserPane::FileSystem => (
                    ctx.apply(Style::default().fg(ctx.state_selected())),
                    ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
                ),
                BrowserPane::TorrentPreview => (
                    ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
                    ctx.apply(Style::default().fg(ctx.state_selected())),
                ),
            }
        } else {
            (
                ctx.apply(Style::default().fg(ctx.state_selected())),
                ctx.apply(Style::default().fg(ctx.accent_sapphire())),
            )
        };

    if let Some(preview_area) = layout.preview {
        draw_torrent_preview_panel(
            f,
            ctx,
            preview_area,
            preview_file_path.map(|p| p.as_path()),
            browser_mode,
            preview_border_style,
            &state.current_path,
        );
    }
    if let Some(search_area) = layout.search {
        let search_block = Block::default()
            .borders(Borders::ALL)
            .border_style(ctx.apply(Style::default().fg(ctx.state_warning())))
            .title(" Search Filter ");
        let search_text = Line::from(vec![
            Span::styled(
                "/",
                ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
            ),
            Span::raw(&app_state.ui.file_browser.search_query),
            Span::styled(
                "_",
                ctx.apply(
                    Style::default()
                        .fg(ctx.state_warning())
                        .add_modifier(Modifier::SLOW_BLINK),
                ),
            ),
        ]);
        f.render_widget(Paragraph::new(search_text).block(search_block), search_area);
    }

    let mut footer_spans = Vec::new();
    match browser_mode {
        FileBrowserMode::ConfigPathSelection { .. } | FileBrowserMode::Directory => {
            footer_spans.push(Span::styled(
                "[Arrows/Vim]",
                ctx.apply(Style::default().fg(ctx.state_info())),
            ));
            footer_spans.push(Span::raw(" Nav | "));
            footer_spans.push(Span::styled(
                "[Backspace]",
                ctx.apply(Style::default().fg(ctx.state_warning())),
            ));
            footer_spans.push(Span::raw(" Up | "));
            footer_spans.push(Span::styled(
                "[Enter]",
                ctx.apply(Style::default().fg(ctx.state_warning())),
            ));
            footer_spans.push(Span::raw(" Down | "));
            footer_spans.push(Span::styled(
                "[Y]",
                ctx.apply(Style::default().fg(ctx.state_success())),
            ));
            footer_spans.push(Span::raw(" Confirm Selection | "));
        }
        FileBrowserMode::DownloadLocSelection {
            focused_pane,
            use_container,
            ..
        } => {
            footer_spans.push(Span::styled(
                "[Tab]",
                ctx.apply(Style::default().fg(ctx.accent_sapphire())),
            ));
            footer_spans.push(Span::raw(" Switch Pane | "));

            if matches!(focused_pane, BrowserPane::TorrentPreview) {
                footer_spans.push(Span::styled(
                    "[Space]",
                    ctx.apply(Style::default().fg(ctx.state_warning())),
                ));
                footer_spans.push(Span::raw(" Priority | "));
            }

            footer_spans.push(Span::styled(
                "[x]",
                ctx.apply(Style::default().fg(ctx.state_selected())),
            ));
            footer_spans.push(Span::raw(" Container Folder | "));

            if *use_container {
                footer_spans.push(Span::styled(
                    "[r]",
                    ctx.apply(Style::default().fg(ctx.accent_sky())),
                ));
                footer_spans.push(Span::raw(" Rename | "));
            }

            footer_spans.push(Span::styled(
                "[Y]",
                ctx.apply(Style::default().fg(ctx.state_success())),
            ));
            footer_spans.push(Span::raw(" Confirm"));
        }
        FileBrowserMode::File(_) => {
            footer_spans.push(Span::styled(
                "[Y]",
                ctx.apply(Style::default().fg(ctx.state_success())),
            ));
            footer_spans.push(Span::raw(" Confirm File | "));
        }
    }
    footer_spans.push(Span::raw(" | "));
    footer_spans.push(Span::styled(
        "[Esc]",
        ctx.apply(Style::default().fg(ctx.state_error())),
    ));
    footer_spans.push(Span::raw(" Cancel"));

    let footer = Paragraph::new(Line::from(footer_spans))
        .alignment(Alignment::Center)
        .style(ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)));
    f.render_widget(footer, layout.footer);

    let inner_height = layout.list.height.saturating_sub(2) as usize;
    let list_width = layout.list.width.saturating_sub(2) as usize;
    let filter = build_filter(browser_mode, &app_state.ui.file_browser.search_query);

    let abs_path = state.current_path.to_string_lossy();
    let item_count = data.len();
    let count_label = if item_count == 0 {
        " (empty)".to_string()
    } else {
        format!(" ({} items)", item_count)
    };
    let left_title = format!(" {}/{} ", abs_path, count_label);
    let right_title = match browser_mode {
        FileBrowserMode::Directory => " Select Directory ".to_string(),
        FileBrowserMode::DownloadLocSelection { .. } => String::new(),
        FileBrowserMode::ConfigPathSelection { .. } => " Select Config Path ".to_string(),
        FileBrowserMode::File(exts) => format!(" Select File [{}] ", exts.join(", ")),
    };

    let visible_items = TreeMathHelper::get_visible_slice(data, state, filter, inner_height);
    let mut list_items = Vec::new();

    if data.is_empty() {
        list_items.push(ListItem::new(Line::from(vec![Span::styled(
            "   (Directory is empty)",
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0))
                .italic(),
        )])));
    } else if visible_items.is_empty() {
        list_items.push(ListItem::new(Line::from(vec![Span::styled(
            format!("   (No matching files among {} items)", item_count),
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0))
                .italic(),
        )])));
    } else {
        for item in visible_items {
            let is_cursor = item.is_cursor;
            let indent_str = "  ".repeat(item.depth);
            let indent_len = indent_str.len();
            let icon_str = if item.node.is_dir {
                ASCII_TREE_DIR_ICON
            } else {
                ASCII_TREE_FILE_ICON
            };
            let icon_len = ASCII_TREE_DIR_ICON.len();

            let (meta_str, meta_len) = if !item.node.is_dir {
                let datetime: chrono::DateTime<chrono::Local> = item.node.payload.modified.into();
                let size_str = format_bytes(item.node.payload.size);
                let s = format!(" {} ({})", size_str, datetime.format("%b %d %H:%M"));
                (s.clone(), s.len())
            } else {
                (String::new(), 0)
            };

            let fixed_used = indent_len + icon_len + meta_len + 1;
            let available_for_name = list_width.saturating_sub(fixed_used);
            let clean_name: String = item
                .node
                .name
                .chars()
                .map(|c| if c.is_control() { '?' } else { c })
                .collect();
            let display_name = truncate_with_ellipsis(&clean_name, available_for_name);

            let (icon_style, text_style) = if is_cursor {
                (
                    Style::default()
                        .fg(ctx.state_warning())
                        .add_modifier(Modifier::BOLD),
                    Style::default()
                        .fg(ctx.state_warning())
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                let i_style = if item.node.is_dir {
                    ctx.apply(Style::default().fg(ctx.state_info()))
                } else {
                    ctx.apply(Style::default().fg(ctx.theme.semantic.text))
                };
                (
                    i_style,
                    ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
                )
            };

            let mut line_spans = vec![
                Span::raw(indent_str),
                Span::styled(icon_str, icon_style),
                Span::styled(display_name, text_style),
            ];

            if !item.node.is_dir {
                line_spans.push(Span::raw(" "));
                line_spans.push(Span::styled(
                    meta_str,
                    ctx.apply(Style::default().fg(ctx.theme.semantic.surface2))
                        .italic(),
                ));
            }

            list_items.push(ListItem::new(Line::from(line_spans)));
        }
    }

    f.render_widget(
        List::new(list_items)
            .block(
                Block::default()
                    .title_top(
                        Line::from(Span::styled(
                            left_title,
                            Style::default().fg(ctx.state_selected()).bold(),
                        ))
                        .alignment(Alignment::Left),
                    )
                    .title_top(
                        Line::from(Span::styled(
                            right_title,
                            Style::default().fg(ctx.state_selected()).italic(),
                        ))
                        .alignment(Alignment::Right),
                    )
                    .borders(Borders::ALL)
                    .border_style(files_border_style),
            )
            .highlight_symbol("▶ "),
        layout.list,
    );
}

fn draw_torrent_preview_panel(
    f: &mut Frame,
    ctx: &ThemeContext,
    area: Rect,
    path: Option<&Path>,
    browser_mode: &FileBrowserMode,
    border_style: Style,
    current_fs_path: &Path,
) {
    let is_narrow = area.width < 50;
    let raw_title = "Torrent Preview";
    let avail_width = area.width.saturating_sub(4) as usize;
    let title = if is_narrow {
        truncate_with_ellipsis("Preview", avail_width)
    } else {
        truncate_with_ellipsis(raw_title, avail_width)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title);

    let inner_area = block.inner(area);
    f.render_widget(block, area);

    if let FileBrowserMode::DownloadLocSelection {
        preview_tree,
        preview_state,
        container_name,
        use_container,
        is_editing_name,
        cursor_pos,
        ..
    } = browser_mode
    {
        let header_lines = if *use_container { 2 } else { 1 };
        let list_height = inner_area.height.saturating_sub(header_lines) as usize;

        let visible_rows = TreeMathHelper::get_visible_slice(
            preview_tree,
            preview_state,
            TreeFilter::default(),
            list_height,
        );

        let mut list_items = Vec::new();
        let root_style = Style::default()
            .fg(ctx.state_info())
            .add_modifier(Modifier::BOLD);

        let path_display = if is_narrow {
            current_fs_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "/".to_string())
        } else {
            current_fs_path.to_string_lossy().to_string()
        };

        list_items.push(ListItem::new(Line::from(vec![
            Span::styled(ASCII_TREE_ROOT_ICON, root_style),
            Span::styled(path_display, root_style),
        ])));

        if *use_container {
            let container_style = if *is_editing_name {
                Style::default()
                    .fg(ctx.accent_sky())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(ctx.state_selected())
                    .add_modifier(Modifier::BOLD)
            };

            let mut spans = vec![
                Span::raw("  "),
                Span::styled(ASCII_TREE_ROOT_ICON, container_style),
            ];

            if *is_editing_name {
                let (before, after) = container_name.split_at(*cursor_pos);
                spans.push(Span::styled(before, container_style));
                spans.push(Span::styled(
                    "█",
                    Style::default()
                        .fg(ctx.accent_sky())
                        .add_modifier(Modifier::SLOW_BLINK),
                ));
                spans.push(Span::styled(after, container_style));
            } else {
                spans.push(Span::styled(container_name.clone(), container_style));
                if !is_narrow {
                    spans.push(Span::styled(
                        " (New)",
                        Style::default()
                            .fg(ctx.theme.semantic.surface2)
                            .add_modifier(Modifier::ITALIC),
                    ));
                }
            }
            list_items.push(ListItem::new(Line::from(spans)));
        }

        let tree_items: Vec<ListItem> = visible_rows
            .iter()
            .map(|item| {
                let is_cursor = item.is_cursor;
                let base_indent_level = if *use_container { 2 } else { 1 };
                let indent_multiplier = if is_narrow { 1 } else { 2 };
                let indent_str = " ".repeat((base_indent_level + item.depth) * indent_multiplier);

                let icon = if item.node.is_dir {
                    ASCII_TREE_DIR_ICON
                } else {
                    ASCII_TREE_FILE_ICON
                };

                let (base_content_style, tag) = match item.node.payload.priority {
                    FilePriority::Skip => (
                        Style::default()
                            .fg(ctx.theme.semantic.surface1)
                            .add_modifier(Modifier::CROSSED_OUT),
                        "[S] ",
                    ),
                    FilePriority::High => (
                        Style::default()
                            .fg(ctx.state_success())
                            .add_modifier(Modifier::BOLD),
                        "[H] ",
                    ),
                    FilePriority::Mixed => (
                        Style::default()
                            .fg(ctx.state_warning())
                            .add_modifier(Modifier::ITALIC),
                        "[*] ",
                    ),
                    FilePriority::Normal => (
                        if item.node.is_dir {
                            ctx.apply(Style::default().fg(ctx.state_info()))
                        } else {
                            ctx.apply(Style::default().fg(ctx.theme.semantic.text))
                        },
                        "",
                    ),
                };

                let final_content_style = if is_cursor {
                    base_content_style
                        .add_modifier(Modifier::BOLD)
                        .add_modifier(Modifier::UNDERLINED)
                } else {
                    base_content_style
                };

                let structure_style = final_content_style
                    .remove_modifier(Modifier::CROSSED_OUT)
                    .remove_modifier(Modifier::UNDERLINED);
                let mut spans = vec![
                    Span::styled(indent_str, structure_style),
                    Span::styled(icon, structure_style),
                    Span::styled(&item.node.name, final_content_style),
                ];

                if !item.node.is_dir {
                    if !is_narrow {
                        spans.push(Span::styled(
                            format!(" ({}) ", format_bytes(item.node.payload.size)),
                            structure_style,
                        ));
                    }
                    if !tag.is_empty() {
                        spans.push(Span::styled(tag, structure_style));
                    }
                }
                ListItem::new(Line::from(spans))
            })
            .collect();

        list_items.extend(tree_items);
        f.render_widget(List::new(list_items), inner_area);
        return;
    }

    if let Some(p) = path {
        let file_bytes = match std::fs::read(p) {
            Ok(b) => b,
            Err(e) => {
                f.render_widget(
                    Paragraph::new(format!("Read Error: {}", e))
                        .style(ctx.apply(Style::default().fg(ctx.state_error()))),
                    inner_area,
                );
                return;
            }
        };

        let torrent = match crate::torrent_file::parser::from_bytes(&file_bytes) {
            Ok(t) => t,
            Err(e) => {
                f.render_widget(
                    Paragraph::new(format!("Invalid Torrent: {}", e))
                        .style(ctx.apply(Style::default().fg(ctx.state_error()))),
                    inner_area,
                );
                return;
            }
        };

        let total_size = torrent.info.total_length();
        let protocol_version = match torrent.info.meta_version {
            Some(2) => {
                if !torrent.info.pieces.is_empty() {
                    "BitTorrent v2 (Hybrid)"
                } else {
                    "BitTorrent v2 (Pure)"
                }
            }
            _ => "BitTorrent v1",
        };
        let info_text = vec![
            Line::from(vec![
                Span::styled(
                    "Name: ",
                    ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
                ),
                Span::raw(&torrent.info.name),
            ]),
            Line::from(vec![
                Span::styled(
                    "Protocol: ",
                    ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
                ),
                Span::styled(
                    protocol_version,
                    Style::default().fg(ctx.state_selected()).bold(),
                ),
            ]),
            Line::from(vec![
                Span::styled(
                    "Size: ",
                    ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
                ),
                Span::raw(format_bytes(total_size as u64)),
            ]),
        ];

        let layout = Layout::vertical([
            Constraint::Length(info_text.len() as u16 + 1),
            Constraint::Min(0),
        ])
        .split(inner_area);
        f.render_widget(
            Paragraph::new(info_text).block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.border))),
            ),
            layout[0],
        );

        let file_list_payloads: Vec<(Vec<String>, TorrentPreviewPayload)> = torrent
            .file_list()
            .into_iter()
            .map(|(path, size)| {
                (
                    path,
                    TorrentPreviewPayload {
                        file_index: None,
                        size,
                        priority: FilePriority::Normal,
                    },
                )
            })
            .collect();

        let final_nodes = RawNode::from_path_list(None, file_list_payloads);
        let mut temp_state = TreeViewState::default();
        for node in &final_nodes {
            node.expand_all(&mut temp_state);
        }

        let visible_rows = TreeMathHelper::get_visible_slice(
            &final_nodes,
            &temp_state,
            TreeFilter::default(),
            layout[1].height as usize,
        );

        let list_items: Vec<ListItem> = visible_rows
            .iter()
            .map(|item| {
                let indent = if is_narrow {
                    " ".repeat(item.depth)
                } else {
                    "  ".repeat(item.depth)
                };
                let icon = if item.node.is_dir {
                    ASCII_TREE_DIR_ICON
                } else {
                    ASCII_TREE_FILE_ICON
                };
                let style = if item.node.is_dir {
                    ctx.apply(Style::default().fg(ctx.state_info()))
                } else {
                    ctx.apply(Style::default().fg(ctx.theme.semantic.text))
                };
                let mut spans = vec![
                    Span::raw(indent),
                    Span::styled(icon, style),
                    Span::styled(&item.node.name, style),
                ];
                if !item.node.is_dir && !is_narrow {
                    spans.push(Span::styled(
                        format!(" ({})", format_bytes(item.node.payload.size)),
                        ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
                    ));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();

        f.render_widget(List::new(list_items), layout[1]);
    }
}

pub async fn handle_event(event: CrosstermEvent, app: &mut App) {
    if !matches!(app.app_state.mode, AppMode::FileBrowser) {
        return;
    }

    if let CrosstermEvent::Key(key) = event {
        if key.kind == KeyEventKind::Press {
            if handle_browser_search_key(key.code, app) {
                return;
            }

            if handle_browser_download_key(key.code, app).await {
                return;
            }

            let _ = handle_browser_common_key(key.code, app).await;
        }
    }
}

fn handle_browser_search_key(key_code: KeyCode, app: &mut App) -> bool {
    if let Some(action) =
        map_search_key_to_browser_action(key_code, app.app_state.ui.file_browser.is_searching)
    {
        let reduced = reduce_browser_action(
            action,
            &mut app.app_state.ui.file_browser.is_searching,
            &mut app.app_state.ui.file_browser.search_query,
        );
        if reduced.redraw {
            app.app_state.ui.needs_redraw = true;
        }
        return true;
    }
    false
}

async fn handle_browser_download_key(key_code: KeyCode, app: &mut App) -> bool {
    let consumed_download_input = {
        let browser_mode = &mut app.app_state.ui.file_browser.browser_mode;
        if let Some(action) = map_download_key_to_action(key_code, browser_mode) {
            reduce_browser_download_action(action, browser_mode).consumed
        } else {
            false
        }
    };
    if consumed_download_input {
        return true;
    }

    if !matches!(
        app.app_state.ui.file_browser.browser_mode,
        FileBrowserMode::DownloadLocSelection { .. }
    ) {
        return false;
    }

    if key_code == KeyCode::Esc {
        let reduced = {
            let file_browser = &app.app_state.ui.file_browser;
            reduce_browser_dialog_action(
                BrowserDialogAction::CancelDownloadSelection,
                &file_browser.state,
                &file_browser.browser_mode,
                !app.app_state.pending_torrent_link.is_empty(),
            )
        };
        execute_browser_dialog_effects(app, reduced.effects).await;
        return true;
    }

    let screen_area = app.app_state.screen_area;
    let is_searching = app.app_state.ui.file_browser.is_searching;
    let consumed_preview_input = {
        let browser_mode = &mut app.app_state.ui.file_browser.browser_mode;
        if let FileBrowserMode::DownloadLocSelection {
            use_container,
            focused_pane,
            preview_tree,
            preview_state,
            ..
        } = browser_mode
        {
            if matches!(focused_pane, BrowserPane::TorrentPreview) {
                let list_height = calculate_preview_list_height(
                    screen_area,
                    is_searching,
                    focused_pane,
                    *use_container,
                );
                reduce_browser_preview_action(
                    map_preview_key_to_action(key_code),
                    preview_state,
                    preview_tree,
                    list_height,
                )
                .consumed
            } else {
                false
            }
        } else {
            false
        }
    };
    if consumed_preview_input {
        return true;
    }

    false
}

async fn handle_browser_common_key(key_code: KeyCode, app: &mut App) -> bool {
    let list_height = {
        let file_browser = &app.app_state.ui.file_browser;
        let has_preview = has_preview_content(
            &file_browser.browser_mode,
            app.app_state.pending_torrent_path.is_some(),
            !app.app_state.pending_torrent_link.is_empty(),
            file_browser.state.cursor_path.as_ref(),
        );
        let pane = focused_pane(&file_browser.browser_mode);
        calculate_list_height(
            app.app_state.screen_area,
            has_preview,
            app.app_state.ui.file_browser.is_searching,
            &pane,
        )
    };

    let consumed_filesystem = {
        let file_browser = &mut app.app_state.ui.file_browser;
        handle_filesystem_navigation(
            key_code,
            BrowserFilesystemNavContext {
                state: &mut file_browser.state,
                data: &file_browser.data,
                browser_mode: &file_browser.browser_mode,
                is_searching: &mut file_browser.is_searching,
                search_query: &mut file_browser.search_query,
                list_height,
                app_command_tx: &app.app_command_tx,
            },
        )
    };
    if consumed_filesystem {
        return true;
    }

    let dialog_action = match key_code {
        KeyCode::Char('Y') => Some(BrowserDialogAction::ConfirmSelection),
        KeyCode::Esc => Some(BrowserDialogAction::Escape),
        _ => None,
    };
    let Some(dialog_action) = dialog_action else {
        return false;
    };

    let reduced = {
        let file_browser = &app.app_state.ui.file_browser;
        reduce_browser_dialog_action(
            dialog_action,
            &file_browser.state,
            &file_browser.browser_mode,
            !app.app_state.pending_torrent_link.is_empty(),
        )
    };
    execute_browser_dialog_effects(app, reduced.effects).await;
    true
}

pub enum ConfirmDecision {
    ToConfig(ConfigUiState),
    Download(DownloadConfirmPayload),
    File(PathBuf),
    None,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BrowserAction {
    Esc,
    Enter,
    Backspace,
    Char(char),
    Noop,
}

pub struct BrowserReduceResult {
    pub redraw: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BrowserFsAction {
    StartSearch,
    Move(TreeAction),
    EnterDir,
    GoParent,
}

pub enum BrowserFsEffect {
    FetchFileTree {
        path: PathBuf,
        browser_mode: FileBrowserMode,
        highlight_path: Option<PathBuf>,
    },
}

pub struct BrowserFsReduceResult {
    pub consumed: bool,
    pub effects: Vec<BrowserFsEffect>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BrowserDialogAction {
    ConfirmSelection,
    Escape,
    CancelDownloadSelection,
}

pub enum BrowserDialogEffect {
    ExecuteConfirmDecision(ConfirmDecision),
    ToConfig(ConfigUiState),
    CleanupPendingLink { async_delete: bool },
    ToNormalAndClearPending,
    ClearSearch,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BrowserTransition {
    ToNormal,
    ToConfig,
}

pub struct BrowserDialogReduceResult {
    pub effects: Vec<BrowserDialogEffect>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BrowserDownloadEditAction {
    Commit,
    Cancel,
    MoveLeft,
    MoveRight,
    Backspace,
    Delete,
    Insert(char),
    Noop,
}

pub struct BrowserDownloadEditReduceResult {
    pub consumed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BrowserDownloadShortcutAction {
    ToggleUseContainer,
    StartRename,
    TogglePane,
}

pub struct BrowserDownloadShortcutReduceResult {
    pub consumed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BrowserDownloadAction {
    Edit(BrowserDownloadEditAction),
    Shortcut(BrowserDownloadShortcutAction),
}

pub struct BrowserDownloadReduceResult {
    pub consumed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BrowserPreviewAction {
    ConfirmSelection,
    Navigate(TreeAction),
    CyclePriority,
    Ignore,
}

pub struct BrowserPreviewReduceResult {
    pub consumed: bool,
}

pub struct BrowserFilesystemNavContext<'a> {
    pub state: &'a mut TreeViewState,
    pub data: &'a [RawNode<FileMetadata>],
    pub browser_mode: &'a FileBrowserMode,
    pub is_searching: &'a mut bool,
    pub search_query: &'a mut String,
    pub list_height: usize,
    pub app_command_tx: &'a mpsc::Sender<AppCommand>,
}

fn map_search_key_to_browser_action(
    key_code: KeyCode,
    is_searching: bool,
) -> Option<BrowserAction> {
    if !is_searching {
        return None;
    }

    Some(match key_code {
        KeyCode::Esc => BrowserAction::Esc,
        KeyCode::Enter => BrowserAction::Enter,
        KeyCode::Backspace => BrowserAction::Backspace,
        KeyCode::Char(c) => BrowserAction::Char(c),
        _ => BrowserAction::Noop,
    })
}

pub fn reduce_browser_action(
    action: BrowserAction,
    is_searching: &mut bool,
    search_query: &mut String,
) -> BrowserReduceResult {
    match action {
        BrowserAction::Esc => {
            *is_searching = false;
            search_query.clear();
        }
        BrowserAction::Enter => {
            *is_searching = false;
        }
        BrowserAction::Backspace => {
            search_query.pop();
        }
        BrowserAction::Char(c) => {
            search_query.push(c);
        }
        BrowserAction::Noop => {}
    }

    BrowserReduceResult { redraw: true }
}

fn map_filesystem_key_to_action(key_code: KeyCode) -> Option<BrowserFsAction> {
    match key_code {
        KeyCode::Char('/') => Some(BrowserFsAction::StartSearch),
        KeyCode::Up | KeyCode::Char('k') => Some(BrowserFsAction::Move(TreeAction::Up)),
        KeyCode::Down | KeyCode::Char('j') => Some(BrowserFsAction::Move(TreeAction::Down)),
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => Some(BrowserFsAction::EnterDir),
        KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('u') => {
            Some(BrowserFsAction::GoParent)
        }
        _ => None,
    }
}

pub fn reduce_filesystem_navigation_action(
    action: BrowserFsAction,
    state: &mut TreeViewState,
    data: &[RawNode<FileMetadata>],
    browser_mode: &FileBrowserMode,
    is_searching: &mut bool,
    search_query: &mut String,
    list_height: usize,
) -> BrowserFsReduceResult {
    let filter = build_filter(browser_mode, search_query);
    let mut result = BrowserFsReduceResult {
        consumed: true,
        effects: Vec::new(),
    };

    match action {
        BrowserFsAction::StartSearch => {
            *is_searching = true;
            search_query.clear();
        }
        BrowserFsAction::Move(tree_action) => {
            TreeMathHelper::apply_action(state, data, tree_action, filter, list_height);
        }
        BrowserFsAction::EnterDir => {
            if let Some(path) = state.cursor_path.clone() {
                if path.is_dir() {
                    *is_searching = false;
                    search_query.clear();
                    result.effects.push(BrowserFsEffect::FetchFileTree {
                        path,
                        browser_mode: browser_mode.clone(),
                        highlight_path: None,
                    });
                }
            }
        }
        BrowserFsAction::GoParent => {
            let child_to_highlight = state.current_path.clone();
            if let Some(parent) = state.current_path.parent() {
                result.effects.push(BrowserFsEffect::FetchFileTree {
                    path: parent.to_path_buf(),
                    browser_mode: browser_mode.clone(),
                    highlight_path: Some(child_to_highlight),
                });
            }
        }
    }

    result
}

fn map_download_name_edit_key_to_action(key_code: KeyCode) -> BrowserDownloadEditAction {
    match key_code {
        KeyCode::Enter => BrowserDownloadEditAction::Commit,
        KeyCode::Esc => BrowserDownloadEditAction::Cancel,
        KeyCode::Left => BrowserDownloadEditAction::MoveLeft,
        KeyCode::Right => BrowserDownloadEditAction::MoveRight,
        KeyCode::Backspace => BrowserDownloadEditAction::Backspace,
        KeyCode::Delete => BrowserDownloadEditAction::Delete,
        KeyCode::Char(c) => BrowserDownloadEditAction::Insert(c),
        _ => BrowserDownloadEditAction::Noop,
    }
}

pub fn map_download_key_to_action(
    key_code: KeyCode,
    browser_mode: &FileBrowserMode,
) -> Option<BrowserDownloadAction> {
    if let FileBrowserMode::DownloadLocSelection {
        is_editing_name,
        use_container,
        ..
    } = browser_mode
    {
        if *is_editing_name {
            return Some(BrowserDownloadAction::Edit(
                map_download_name_edit_key_to_action(key_code),
            ));
        }

        if let Some(action) = map_download_shortcut_key_to_action(key_code, *use_container) {
            return Some(BrowserDownloadAction::Shortcut(action));
        }
    }
    None
}

pub fn reduce_download_name_edit_action(
    action: BrowserDownloadEditAction,
    container_name: &mut String,
    is_editing_name: &mut bool,
    cursor_pos: &mut usize,
    original_name_backup: &str,
) -> BrowserDownloadEditReduceResult {
    match action {
        BrowserDownloadEditAction::Commit => {
            *is_editing_name = false;
        }
        BrowserDownloadEditAction::Cancel => {
            *container_name = original_name_backup.to_string();
            *is_editing_name = false;
            *cursor_pos = container_name.len();
        }
        BrowserDownloadEditAction::MoveLeft => {
            *cursor_pos = cursor_pos.saturating_sub(1);
        }
        BrowserDownloadEditAction::MoveRight => {
            if *cursor_pos < container_name.len() {
                *cursor_pos += 1;
            }
        }
        BrowserDownloadEditAction::Backspace => {
            if *cursor_pos > 0 {
                container_name.remove(*cursor_pos - 1);
                *cursor_pos -= 1;
            }
        }
        BrowserDownloadEditAction::Delete => {
            if *cursor_pos < container_name.len() {
                container_name.remove(*cursor_pos);
            }
        }
        BrowserDownloadEditAction::Insert(c) => {
            container_name.insert(*cursor_pos, c);
            *cursor_pos += 1;
        }
        BrowserDownloadEditAction::Noop => {}
    }

    BrowserDownloadEditReduceResult { consumed: true }
}

fn map_download_shortcut_key_to_action(
    key_code: KeyCode,
    use_container: bool,
) -> Option<BrowserDownloadShortcutAction> {
    match key_code {
        KeyCode::Char('x') => Some(BrowserDownloadShortcutAction::ToggleUseContainer),
        KeyCode::Char('r') if use_container => Some(BrowserDownloadShortcutAction::StartRename),
        KeyCode::Tab => Some(BrowserDownloadShortcutAction::TogglePane),
        _ => None,
    }
}

pub fn reduce_download_shortcut_action(
    action: BrowserDownloadShortcutAction,
    container_name: &str,
    use_container: &mut bool,
    is_editing_name: &mut bool,
    focused_pane: &mut BrowserPane,
    cursor_pos: &mut usize,
    original_name_backup: &mut String,
) -> BrowserDownloadShortcutReduceResult {
    match action {
        BrowserDownloadShortcutAction::ToggleUseContainer => {
            *use_container = !*use_container;
        }
        BrowserDownloadShortcutAction::StartRename => {
            *is_editing_name = true;
            *original_name_backup = container_name.to_string();
            *cursor_pos = container_name.len();
            *focused_pane = BrowserPane::TorrentPreview;
        }
        BrowserDownloadShortcutAction::TogglePane => {
            *focused_pane = match focused_pane {
                BrowserPane::FileSystem => BrowserPane::TorrentPreview,
                BrowserPane::TorrentPreview => BrowserPane::FileSystem,
            };
        }
    }

    BrowserDownloadShortcutReduceResult { consumed: true }
}

pub fn reduce_browser_download_action(
    action: BrowserDownloadAction,
    browser_mode: &mut FileBrowserMode,
) -> BrowserDownloadReduceResult {
    if let FileBrowserMode::DownloadLocSelection {
        container_name,
        use_container,
        is_editing_name,
        focused_pane,
        cursor_pos,
        original_name_backup,
        ..
    } = browser_mode
    {
        let consumed = match action {
            BrowserDownloadAction::Edit(edit_action) => {
                reduce_download_name_edit_action(
                    edit_action,
                    container_name,
                    is_editing_name,
                    cursor_pos,
                    original_name_backup,
                )
                .consumed
            }
            BrowserDownloadAction::Shortcut(shortcut_action) => {
                reduce_download_shortcut_action(
                    shortcut_action,
                    container_name,
                    use_container,
                    is_editing_name,
                    focused_pane,
                    cursor_pos,
                    original_name_backup,
                )
                .consumed
            }
        };

        return BrowserDownloadReduceResult { consumed };
    }

    BrowserDownloadReduceResult { consumed: false }
}

pub fn has_preview_content(
    browser_mode: &FileBrowserMode,
    pending_torrent_path: bool,
    pending_torrent_link: bool,
    cursor_path: Option<&std::path::PathBuf>,
) -> bool {
    match browser_mode {
        FileBrowserMode::DownloadLocSelection { .. } => {
            pending_torrent_path || pending_torrent_link
        }
        FileBrowserMode::File(_) => {
            cursor_path.is_some_and(|p| p.extension().is_some_and(|ext| ext == "torrent"))
        }
        _ => false,
    }
}

pub fn focused_pane(browser_mode: &FileBrowserMode) -> BrowserPane {
    if let FileBrowserMode::DownloadLocSelection { focused_pane, .. } = browser_mode {
        focused_pane.clone()
    } else {
        BrowserPane::FileSystem
    }
}

pub fn calculate_area(screen: Rect, has_preview_content: bool) -> Rect {
    if has_preview_content {
        if screen.width < 60 {
            screen
        } else {
            centered_rect(90, 80, screen)
        }
    } else if screen.width < 40 {
        screen
    } else {
        centered_rect(75, 80, screen)
    }
}

pub fn calculate_list_height(
    screen: Rect,
    has_preview_content: bool,
    is_searching: bool,
    focused_pane: &BrowserPane,
) -> usize {
    let area = calculate_area(screen, has_preview_content);
    let layout =
        calculate_file_browser_layout(area, has_preview_content, is_searching, focused_pane);
    layout.list.height.saturating_sub(2) as usize
}

pub fn calculate_preview_list_height(
    screen: Rect,
    is_searching: bool,
    focused_pane: &BrowserPane,
    use_container: bool,
) -> Option<usize> {
    let area = if screen.width < 60 {
        screen
    } else {
        centered_rect(90, 80, screen)
    };
    let layout = calculate_file_browser_layout(area, true, is_searching, focused_pane);
    layout.preview.map(|preview_rect| {
        let inner_height = preview_rect.height.saturating_sub(2);
        let header_rows = if use_container { 2 } else { 1 };
        inner_height.saturating_sub(header_rows) as usize
    })
}

pub fn map_preview_key_to_action(key_code: KeyCode) -> BrowserPreviewAction {
    match key_code {
        KeyCode::Char('Y') => BrowserPreviewAction::ConfirmSelection,
        KeyCode::Up | KeyCode::Char('k') => BrowserPreviewAction::Navigate(TreeAction::Up),
        KeyCode::Down | KeyCode::Char('j') => BrowserPreviewAction::Navigate(TreeAction::Down),
        KeyCode::Left | KeyCode::Char('h') => BrowserPreviewAction::Navigate(TreeAction::Left),
        KeyCode::Right | KeyCode::Char('l') => BrowserPreviewAction::Navigate(TreeAction::Right),
        KeyCode::Char(' ') => BrowserPreviewAction::CyclePriority,
        _ => BrowserPreviewAction::Ignore,
    }
}

pub fn reduce_browser_preview_action(
    action: BrowserPreviewAction,
    preview_state: &mut TreeViewState,
    preview_tree: &mut [RawNode<TorrentPreviewPayload>],
    list_height: Option<usize>,
) -> BrowserPreviewReduceResult {
    match action {
        BrowserPreviewAction::ConfirmSelection => BrowserPreviewReduceResult { consumed: false },
        BrowserPreviewAction::Navigate(tree_action) => {
            if let Some(height) = list_height {
                TreeMathHelper::apply_action(
                    preview_state,
                    preview_tree,
                    tree_action,
                    TreeFilter::default(),
                    height,
                );
            }
            BrowserPreviewReduceResult { consumed: true }
        }
        BrowserPreviewAction::CyclePriority => {
            if let Some(_height) = list_height {
                if let Some(target) = &preview_state.cursor_path {
                    apply_priority_cycle(preview_tree, target);
                }
            }
            BrowserPreviewReduceResult { consumed: true }
        }
        BrowserPreviewAction::Ignore => BrowserPreviewReduceResult { consumed: true },
    }
}

pub fn build_filter(
    browser_mode: &FileBrowserMode,
    search_query: &str,
) -> TreeFilter<FileMetadata> {
    match browser_mode {
        FileBrowserMode::Directory
        | FileBrowserMode::DownloadLocSelection { .. }
        | FileBrowserMode::ConfigPathSelection { .. } => TreeFilter::from_text(search_query),
        FileBrowserMode::File(extensions) => {
            let exts = extensions.clone();
            TreeFilter::new(search_query, move |node| {
                node.is_dir || exts.iter().any(|ext| node.name.ends_with(ext))
            })
        }
    }
}

pub fn handle_filesystem_navigation(
    key_code: KeyCode,
    ctx: BrowserFilesystemNavContext<'_>,
) -> bool {
    if let Some(action) = map_filesystem_key_to_action(key_code) {
        let reduced = reduce_filesystem_navigation_action(
            action,
            ctx.state,
            ctx.data,
            ctx.browser_mode,
            ctx.is_searching,
            ctx.search_query,
            ctx.list_height,
        );
        for effect in reduced.effects {
            match effect {
                BrowserFsEffect::FetchFileTree {
                    path,
                    browser_mode,
                    highlight_path,
                } => {
                    let _ = ctx.app_command_tx.try_send(AppCommand::FetchFileTree {
                        path,
                        browser_mode,
                        highlight_path,
                    });
                }
            }
        }
        reduced.consumed
    } else {
        false
    }
}

pub fn reduce_browser_dialog_action(
    action: BrowserDialogAction,
    state: &TreeViewState,
    browser_mode: &FileBrowserMode,
    has_pending_torrent_link: bool,
) -> BrowserDialogReduceResult {
    let mut result = BrowserDialogReduceResult {
        effects: Vec::new(),
    };

    match action {
        BrowserDialogAction::ConfirmSelection => {
            result
                .effects
                .push(BrowserDialogEffect::ExecuteConfirmDecision(
                    resolve_confirm_decision(state, browser_mode),
                ));
            result.effects.push(BrowserDialogEffect::ClearSearch);
        }
        BrowserDialogAction::Escape => {
            if let Some(config_ui) = escape_to_config_mode(browser_mode) {
                result.effects.push(BrowserDialogEffect::ClearSearch);
                result
                    .effects
                    .push(BrowserDialogEffect::ToConfig(config_ui));
                return result;
            }

            if matches!(browser_mode, FileBrowserMode::DownloadLocSelection { .. })
                && has_pending_torrent_link
            {
                result
                    .effects
                    .push(BrowserDialogEffect::CleanupPendingLink {
                        async_delete: false,
                    });
            }

            result.effects.push(BrowserDialogEffect::ClearSearch);
            result
                .effects
                .push(BrowserDialogEffect::ToNormalAndClearPending);
        }
        BrowserDialogAction::CancelDownloadSelection => {
            if has_pending_torrent_link {
                result
                    .effects
                    .push(BrowserDialogEffect::CleanupPendingLink { async_delete: true });
            }
            result.effects.push(BrowserDialogEffect::ClearSearch);
            result
                .effects
                .push(BrowserDialogEffect::ToNormalAndClearPending);
        }
    }

    result
}

pub async fn execute_browser_dialog_effects(app: &mut App, effects: Vec<BrowserDialogEffect>) {
    for effect in effects {
        match effect {
            BrowserDialogEffect::ExecuteConfirmDecision(decision) => {
                if let Some(transition) = execute_confirm_decision(app, decision).await {
                    apply_browser_transition(app, transition);
                }
            }
            BrowserDialogEffect::ToConfig(config_ui) => {
                app.app_state.ui.config = config_ui;
                apply_browser_transition(app, BrowserTransition::ToConfig);
            }
            BrowserDialogEffect::CleanupPendingLink { async_delete } => {
                cleanup_pending_link_on_escape(
                    &app.app_state.pending_torrent_link,
                    &mut app.torrent_manager_command_txs,
                    &mut app.app_state.torrents,
                    &mut app.app_state.torrent_list_order,
                    async_delete,
                );
            }
            BrowserDialogEffect::ToNormalAndClearPending => {
                apply_browser_transition(app, BrowserTransition::ToNormal);
                app.app_state.pending_torrent_path = None;
                app.app_state.pending_torrent_link.clear();
            }
            BrowserDialogEffect::ClearSearch => {
                app.app_state.ui.file_browser.is_searching = false;
                app.app_state.ui.file_browser.search_query.clear();
            }
        }
    }
}

fn apply_browser_transition(app: &mut App, transition: BrowserTransition) {
    match transition {
        BrowserTransition::ToNormal => app.app_state.mode = AppMode::Normal,
        BrowserTransition::ToConfig => app.app_state.mode = AppMode::Config,
    }
}

pub fn confirm_config_path_selection(
    state: &TreeViewState,
    browser_mode: &FileBrowserMode,
) -> Option<ConfigUiState> {
    if let FileBrowserMode::ConfigPathSelection {
        target_item,
        current_settings,
        selected_index,
        items,
    } = browser_mode
    {
        let mut new_settings = current_settings.clone();
        let selected_path = state.current_path.clone();

        match target_item {
            ConfigItem::DefaultDownloadFolder if !crate::config::is_shared_config_mode() => {
                new_settings.default_download_folder = Some(selected_path)
            }
            ConfigItem::WatchFolder => new_settings.watch_folder = Some(selected_path),
            _ => {}
        }

        return Some(ConfigUiState {
            settings_edit: new_settings,
            selected_index: *selected_index,
            items: items.clone(),
            editing: None,
        });
    }
    None
}

pub fn escape_to_config_mode(browser_mode: &FileBrowserMode) -> Option<ConfigUiState> {
    if let FileBrowserMode::ConfigPathSelection {
        current_settings,
        selected_index,
        items,
        ..
    } = browser_mode
    {
        return Some(ConfigUiState {
            settings_edit: current_settings.clone(),
            selected_index: *selected_index,
            items: items.clone(),
            editing: None,
        });
    }
    None
}

pub fn selected_torrent_file_for_confirm(
    state: &TreeViewState,
    browser_mode: &FileBrowserMode,
) -> Option<std::path::PathBuf> {
    if let FileBrowserMode::File(extensions) = browser_mode {
        if let Some(path) = state.cursor_path.clone() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if extensions.iter().any(|ext| name.ends_with(ext)) {
                return Some(path);
            }
        }
    }
    None
}

pub fn resolve_confirm_decision(
    state: &TreeViewState,
    browser_mode: &FileBrowserMode,
) -> ConfirmDecision {
    if let Some(config_ui) = confirm_config_path_selection(state, browser_mode) {
        return ConfirmDecision::ToConfig(config_ui);
    }
    if let Some(payload) = build_download_confirm_payload(state, browser_mode) {
        return ConfirmDecision::Download(payload);
    }
    if let Some(path) = selected_torrent_file_for_confirm(state, browser_mode) {
        return ConfirmDecision::File(path);
    }
    ConfirmDecision::None
}

pub async fn execute_confirm_decision(
    app: &mut App,
    decision: ConfirmDecision,
) -> Option<BrowserTransition> {
    match decision {
        ConfirmDecision::ToConfig(config_ui) => {
            tracing::info!(target: "superseedr", "Confirming Config Path Selection");
            app.app_state.ui.config = config_ui;
            Some(BrowserTransition::ToConfig)
        }
        ConfirmDecision::Download(payload) => {
            if let Some(pending_path) = app.app_state.pending_torrent_path.take() {
                match app.prepare_add_torrent_file_request(
                    pending_path,
                    Some(payload.base_path.clone()),
                    payload.container_name_to_use.clone(),
                    payload.file_priorities.clone(),
                ) {
                    Ok(request) => {
                        let _ = app
                            .app_command_tx
                            .send(AppCommand::SubmitControlRequest(request))
                            .await;
                    }
                    Err(error) => {
                        app.app_state.system_error = Some(error);
                    }
                }
            } else if !app.app_state.pending_torrent_link.is_empty() {
                let request = app.prepare_add_magnet_request(
                    app.app_state.pending_torrent_link.clone(),
                    Some(payload.base_path),
                    payload.container_name_to_use,
                    payload.file_priorities,
                );
                let _ = app
                    .app_command_tx
                    .send(AppCommand::SubmitControlRequest(request))
                    .await;
                app.app_state.pending_torrent_link.clear();
            } else {
                tracing::warn!(target: "superseedr", "SHIFT+Y pressed but no pending content was found");
            }
            Some(BrowserTransition::ToNormal)
        }
        ConfirmDecision::File(path) => {
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.ends_with(".torrent"))
            {
                let _ = app
                    .app_command_tx
                    .send(AppCommand::AddTorrentFromFile(path))
                    .await;
            }
            Some(BrowserTransition::ToNormal)
        }
        ConfirmDecision::None => None,
    }
}

pub fn build_download_confirm_payload(
    state: &TreeViewState,
    browser_mode: &FileBrowserMode,
) -> Option<DownloadConfirmPayload> {
    if let FileBrowserMode::DownloadLocSelection {
        container_name,
        use_container,
        preview_tree,
        ..
    } = browser_mode
    {
        let base_path = state.current_path.clone();
        let container_name_to_use = if *use_container {
            Some(container_name.clone())
        } else {
            Some(String::new())
        };

        let mut file_priorities = HashMap::new();
        for node in preview_tree {
            node.collect_priorities(&mut file_priorities);
        }

        return Some(DownloadConfirmPayload {
            base_path,
            container_name_to_use,
            file_priorities,
        });
    }
    None
}

pub fn pending_link_info_hash(pending_torrent_link: &str) -> Option<Vec<u8>> {
    if pending_torrent_link.is_empty() {
        return None;
    }
    crate::app::parse_hybrid_hashes(pending_torrent_link).0
}

pub fn cleanup_pending_link_on_escape(
    pending_torrent_link: &str,
    torrent_manager_command_txs: &mut HashMap<Vec<u8>, Sender<ManagerCommand>>,
    torrents: &mut HashMap<Vec<u8>, TorrentDisplayState>,
    torrent_list_order: &mut Vec<Vec<u8>>,
    async_delete: bool,
) {
    if let Some(info_hash) = pending_link_info_hash(pending_torrent_link) {
        if async_delete {
            if let Some(manager_tx) = torrent_manager_command_txs.get(&info_hash) {
                let tx = manager_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = tx.send(ManagerCommand::DeleteFile).await {
                        tracing::error!("Failed to send DeleteFile to cancelled manager: {}", e);
                    }
                });
            }
            torrent_manager_command_txs.remove(&info_hash);
        } else if let Some(manager_tx) = torrent_manager_command_txs.remove(&info_hash) {
            let _ = manager_tx.try_send(ManagerCommand::DeleteFile);
        }

        torrents.remove(&info_hash);
        torrent_list_order.retain(|h| h != &info_hash);
    }
}

pub fn apply_priority_cycle(
    nodes: &mut [RawNode<TorrentPreviewPayload>],
    target_path: &Path,
) -> bool {
    for node in nodes {
        let found = node.find_and_act(target_path, &mut |target_node| {
            let new_priority = target_node.payload.priority.next();
            target_node.apply_recursive(&|n| {
                n.payload.priority = new_priority;
            });
        });

        if found {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{BrowserPane, ConfigItem, TorrentPreviewPayload};
    use crate::tui::tree::{RawNode, TreeViewState};
    use std::path::PathBuf;

    #[test]
    fn search_reducer_clears_on_escape() {
        let mut is_searching = true;
        let mut query = String::from("abc");
        let action = map_search_key_to_browser_action(KeyCode::Esc, is_searching)
            .expect("expected search action");
        let out = reduce_browser_action(action, &mut is_searching, &mut query);
        assert!(out.redraw);
        assert!(!is_searching);
        assert!(query.is_empty());
    }

    #[test]
    fn reducer_search_char_appends_and_consumes() {
        let mut is_searching = true;
        let mut query = String::from("ab");

        let out = reduce_browser_action(BrowserAction::Char('c'), &mut is_searching, &mut query);

        assert!(out.redraw);
        assert!(is_searching);
        assert_eq!(query, "abc");
    }

    #[test]
    fn reducer_search_noop_still_consumes_when_searching() {
        let mut is_searching = true;
        let mut query = String::from("abc");

        let out = reduce_browser_action(BrowserAction::Noop, &mut is_searching, &mut query);

        assert!(out.redraw);
        assert!(is_searching);
        assert_eq!(query, "abc");
    }

    #[test]
    fn reducer_filesystem_start_search_sets_flag_and_clears_query() {
        let mut is_searching = false;
        let mut query = String::from("abc");
        let mut state = TreeViewState::default();
        let data: Vec<RawNode<FileMetadata>> = vec![];
        let mode = FileBrowserMode::Directory;

        let out = reduce_filesystem_navigation_action(
            BrowserFsAction::StartSearch,
            &mut state,
            &data,
            &mode,
            &mut is_searching,
            &mut query,
            5,
        );

        assert!(out.consumed);
        assert!(is_searching);
        assert!(query.is_empty());
    }

    #[test]
    fn reducer_filesystem_enter_dir_emits_fetch_effect() {
        let mut is_searching = true;
        let mut query = String::from("abc");
        let mut state = TreeViewState {
            current_path: PathBuf::from("."),
            cursor_path: Some(PathBuf::from(".")),
            ..Default::default()
        };
        let data: Vec<RawNode<FileMetadata>> = vec![];
        let mode = FileBrowserMode::Directory;

        let out = reduce_filesystem_navigation_action(
            BrowserFsAction::EnterDir,
            &mut state,
            &data,
            &mode,
            &mut is_searching,
            &mut query,
            5,
        );

        assert!(out.consumed);
        assert!(!is_searching);
        assert!(query.is_empty());
        assert_eq!(out.effects.len(), 1);
        assert!(matches!(
            out.effects[0],
            BrowserFsEffect::FetchFileTree { ref path, highlight_path: None, .. }
                if path == &PathBuf::from(".")
        ));
    }

    #[test]
    fn reducer_download_edit_insert_updates_buffer_and_cursor() {
        let mut mode = FileBrowserMode::DownloadLocSelection {
            torrent_files: vec![],
            container_name: "ab".to_string(),
            use_container: true,
            is_editing_name: true,
            focused_pane: BrowserPane::TorrentPreview,
            preview_tree: vec![RawNode {
                name: "x".to_string(),
                full_path: PathBuf::from("x"),
                children: vec![],
                payload: TorrentPreviewPayload::default(),
                is_dir: false,
            }],
            preview_state: TreeViewState::default(),
            cursor_pos: 2,
            original_name_backup: "ab".to_string(),
        };

        let out = reduce_browser_download_action(
            BrowserDownloadAction::Edit(BrowserDownloadEditAction::Insert('c')),
            &mut mode,
        );
        assert!(out.consumed);
        match mode {
            FileBrowserMode::DownloadLocSelection {
                container_name,
                cursor_pos,
                ..
            } => {
                assert_eq!(container_name, "abc");
                assert_eq!(cursor_pos, 3);
            }
            _ => panic!("expected DownloadLocSelection"),
        }
    }

    #[test]
    fn reducer_download_edit_cancel_restores_backup() {
        let mut name = String::from("abc");
        let mut is_editing_name = true;
        let mut cursor_pos = 3;
        let backup = String::from("orig");

        let out = reduce_download_name_edit_action(
            BrowserDownloadEditAction::Cancel,
            &mut name,
            &mut is_editing_name,
            &mut cursor_pos,
            &backup,
        );

        assert!(out.consumed);
        assert_eq!(name, "orig");
        assert!(!is_editing_name);
        assert_eq!(cursor_pos, 4);
    }

    #[test]
    fn reducer_download_shortcut_start_rename_sets_editing_state() {
        let mut use_container = true;
        let mut is_editing_name = false;
        let mut focused_pane = BrowserPane::FileSystem;
        let mut cursor_pos = 0;
        let mut original_name_backup = String::new();
        let container_name = String::from("seed");

        let out = reduce_download_shortcut_action(
            BrowserDownloadShortcutAction::StartRename,
            &container_name,
            &mut use_container,
            &mut is_editing_name,
            &mut focused_pane,
            &mut cursor_pos,
            &mut original_name_backup,
        );

        assert!(out.consumed);
        assert!(is_editing_name);
        assert_eq!(original_name_backup, "seed");
        assert_eq!(cursor_pos, 4);
        assert_eq!(focused_pane, BrowserPane::TorrentPreview);
    }

    #[test]
    fn map_download_shortcut_requires_container_for_rename() {
        let action = map_download_shortcut_key_to_action(KeyCode::Char('r'), false);
        assert!(action.is_none());
    }

    #[test]
    fn map_download_key_prefers_edit_action_while_editing() {
        let mode = FileBrowserMode::DownloadLocSelection {
            torrent_files: vec![],
            container_name: "x".to_string(),
            use_container: true,
            is_editing_name: true,
            focused_pane: BrowserPane::FileSystem,
            preview_tree: vec![],
            preview_state: TreeViewState::default(),
            cursor_pos: 1,
            original_name_backup: "x".to_string(),
        };

        let action = map_download_key_to_action(KeyCode::Tab, &mode);

        assert!(matches!(
            action,
            Some(BrowserDownloadAction::Edit(BrowserDownloadEditAction::Noop))
        ));
    }

    #[test]
    fn reduce_browser_download_shortcut_updates_mode() {
        let mut mode = FileBrowserMode::DownloadLocSelection {
            torrent_files: vec![],
            container_name: "seed".to_string(),
            use_container: true,
            is_editing_name: false,
            focused_pane: BrowserPane::FileSystem,
            preview_tree: vec![],
            preview_state: TreeViewState::default(),
            cursor_pos: 4,
            original_name_backup: String::new(),
        };

        let out = reduce_browser_download_action(
            BrowserDownloadAction::Shortcut(BrowserDownloadShortcutAction::StartRename),
            &mut mode,
        );

        assert!(out.consumed);
        match mode {
            FileBrowserMode::DownloadLocSelection {
                is_editing_name,
                focused_pane,
                original_name_backup,
                ..
            } => {
                assert!(is_editing_name);
                assert_eq!(focused_pane, BrowserPane::TorrentPreview);
                assert_eq!(original_name_backup, "seed");
            }
            _ => panic!("expected DownloadLocSelection"),
        }
    }

    #[test]
    fn name_edit_guard_ignored_when_not_editing() {
        let mut mode = FileBrowserMode::ConfigPathSelection {
            target_item: ConfigItem::WatchFolder,
            current_settings: Box::default(),
            selected_index: 0,
            items: vec![],
        };
        let out = reduce_browser_download_action(
            BrowserDownloadAction::Edit(BrowserDownloadEditAction::Insert('x')),
            &mut mode,
        );
        assert!(!out.consumed);
    }

    #[test]
    fn reducer_download_shortcuts_toggle_pane() {
        let mut mode = FileBrowserMode::DownloadLocSelection {
            torrent_files: vec![],
            container_name: "x".to_string(),
            use_container: true,
            is_editing_name: false,
            focused_pane: BrowserPane::FileSystem,
            preview_tree: vec![],
            preview_state: TreeViewState::default(),
            cursor_pos: 1,
            original_name_backup: "x".to_string(),
        };
        let out = reduce_browser_download_action(
            BrowserDownloadAction::Shortcut(BrowserDownloadShortcutAction::TogglePane),
            &mut mode,
        );
        assert!(out.consumed);
        match mode {
            FileBrowserMode::DownloadLocSelection { focused_pane, .. } => {
                assert_eq!(focused_pane, BrowserPane::TorrentPreview);
            }
            _ => panic!("expected DownloadLocSelection"),
        }
    }

    #[test]
    fn has_preview_content_matches_file_mode_torrent_extension() {
        let mode = FileBrowserMode::File(vec![".torrent".to_string()]);
        let path = PathBuf::from("demo.torrent");
        assert!(has_preview_content(&mode, false, false, Some(&path)));
    }

    #[test]
    fn preview_reducer_navigate_consumes_direction_key() {
        let mut tree = vec![RawNode {
            name: "root".to_string(),
            full_path: PathBuf::from("root"),
            children: vec![RawNode {
                name: "child".to_string(),
                full_path: PathBuf::from("root/child"),
                children: vec![],
                payload: TorrentPreviewPayload::default(),
                is_dir: false,
            }],
            payload: TorrentPreviewPayload::default(),
            is_dir: true,
        }];
        let mut state = TreeViewState::default();
        state.expanded_paths.insert(PathBuf::from("root"));
        state.cursor_path = Some(PathBuf::from("root"));
        let out = reduce_browser_preview_action(
            map_preview_key_to_action(KeyCode::Down),
            &mut state,
            &mut tree,
            Some(10),
        );
        assert!(out.consumed);
        assert_eq!(state.cursor_path, Some(PathBuf::from("root/child")));
    }

    #[test]
    fn preview_reducer_passes_through_confirm_key() {
        let mut tree: Vec<RawNode<TorrentPreviewPayload>> = vec![];
        let mut state = TreeViewState::default();
        let out = reduce_browser_preview_action(
            map_preview_key_to_action(KeyCode::Char('Y')),
            &mut state,
            &mut tree,
            Some(10),
        );
        assert!(!out.consumed);
    }

    #[test]
    fn preview_reducer_ignores_unknown_key_with_consume() {
        let mut tree: Vec<RawNode<TorrentPreviewPayload>> = vec![];
        let mut state = TreeViewState::default();
        let out = reduce_browser_preview_action(
            map_preview_key_to_action(KeyCode::Char('z')),
            &mut state,
            &mut tree,
            Some(10),
        );
        assert!(out.consumed);
    }

    #[test]
    fn preview_reducer_cycles_priority_on_space() {
        let mut tree = vec![RawNode {
            name: "root".to_string(),
            full_path: PathBuf::from("root"),
            children: vec![],
            payload: TorrentPreviewPayload::default(),
            is_dir: true,
        }];
        let mut state = TreeViewState {
            cursor_path: Some(PathBuf::from("root")),
            ..Default::default()
        };

        let out = reduce_browser_preview_action(
            map_preview_key_to_action(KeyCode::Char(' ')),
            &mut state,
            &mut tree,
            Some(10),
        );

        assert!(out.consumed);
        assert_eq!(tree[0].payload.priority, FilePriority::Skip);
    }

    #[test]
    fn filesystem_navigation_starts_search() {
        let mut state = TreeViewState::default();
        let data: Vec<RawNode<FileMetadata>> = vec![];
        let mode = FileBrowserMode::Directory;
        let (tx, _rx) = mpsc::channel(1);
        let mut is_searching = false;
        let mut query = String::from("abc");
        let consumed = handle_filesystem_navigation(
            KeyCode::Char('/'),
            BrowserFilesystemNavContext {
                state: &mut state,
                data: &data,
                browser_mode: &mode,
                is_searching: &mut is_searching,
                search_query: &mut query,
                list_height: 5,
                app_command_tx: &tx,
            },
        );
        assert!(consumed);
        assert!(is_searching);
        assert!(query.is_empty());
    }

    #[test]
    fn confirm_config_path_selection_returns_config_mode() {
        let mode = FileBrowserMode::ConfigPathSelection {
            target_item: ConfigItem::WatchFolder,
            current_settings: Box::default(),
            selected_index: 2,
            items: vec![ConfigItem::WatchFolder],
        };
        let state = TreeViewState {
            current_path: PathBuf::from("/tmp"),
            ..Default::default()
        };
        let out = confirm_config_path_selection(&state, &mode);
        assert!(matches!(out, Some(ConfigUiState { .. })));
    }

    #[test]
    fn resolve_confirm_decision_prefers_config_path_mode() {
        let mode = FileBrowserMode::ConfigPathSelection {
            target_item: ConfigItem::WatchFolder,
            current_settings: Box::default(),
            selected_index: 0,
            items: vec![ConfigItem::WatchFolder],
        };
        let state = TreeViewState {
            current_path: PathBuf::from("/tmp"),
            ..Default::default()
        };
        let decision = resolve_confirm_decision(&state, &mode);
        assert!(matches!(
            decision,
            ConfirmDecision::ToConfig(ConfigUiState { .. })
        ));
    }

    #[test]
    fn reducer_dialog_confirm_emits_execute_and_clear_search() {
        let mode = FileBrowserMode::Directory;
        let state = TreeViewState::default();

        let out = reduce_browser_dialog_action(
            BrowserDialogAction::ConfirmSelection,
            &state,
            &mode,
            false,
        );

        assert_eq!(out.effects.len(), 2);
        assert!(matches!(
            out.effects[0],
            BrowserDialogEffect::ExecuteConfirmDecision(_)
        ));
        assert!(matches!(out.effects[1], BrowserDialogEffect::ClearSearch));
    }

    #[test]
    fn reducer_dialog_escape_prefers_config_switch() {
        let mode = FileBrowserMode::ConfigPathSelection {
            target_item: ConfigItem::WatchFolder,
            current_settings: Box::default(),
            selected_index: 0,
            items: vec![ConfigItem::WatchFolder],
        };
        let state = TreeViewState::default();

        let out = reduce_browser_dialog_action(BrowserDialogAction::Escape, &state, &mode, true);

        assert_eq!(out.effects.len(), 2);
        assert!(matches!(out.effects[0], BrowserDialogEffect::ClearSearch));
        assert!(matches!(
            out.effects[1],
            BrowserDialogEffect::ToConfig(ConfigUiState { .. })
        ));
    }

    #[test]
    fn reducer_dialog_escape_directory_clears_search_and_exits_without_cleanup() {
        let mode = FileBrowserMode::Directory;
        let state = TreeViewState::default();

        let out = reduce_browser_dialog_action(BrowserDialogAction::Escape, &state, &mode, true);

        assert_eq!(out.effects.len(), 2);
        assert!(matches!(out.effects[0], BrowserDialogEffect::ClearSearch));
        assert!(matches!(
            out.effects[1],
            BrowserDialogEffect::ToNormalAndClearPending
        ));
    }

    #[test]
    fn reducer_dialog_escape_download_with_pending_cleans_then_exits() {
        let mode = FileBrowserMode::DownloadLocSelection {
            torrent_files: vec![],
            container_name: "x".to_string(),
            use_container: true,
            is_editing_name: false,
            focused_pane: BrowserPane::FileSystem,
            preview_tree: vec![],
            preview_state: TreeViewState::default(),
            cursor_pos: 1,
            original_name_backup: "x".to_string(),
        };
        let state = TreeViewState::default();

        let out = reduce_browser_dialog_action(BrowserDialogAction::Escape, &state, &mode, true);

        assert_eq!(out.effects.len(), 3);
        assert!(matches!(
            out.effects[0],
            BrowserDialogEffect::CleanupPendingLink {
                async_delete: false
            }
        ));
        assert!(matches!(out.effects[1], BrowserDialogEffect::ClearSearch));
        assert!(matches!(
            out.effects[2],
            BrowserDialogEffect::ToNormalAndClearPending
        ));
    }

    #[test]
    fn reducer_dialog_cancel_download_emits_async_cleanup_and_exit() {
        let mode = FileBrowserMode::DownloadLocSelection {
            torrent_files: vec![],
            container_name: "x".to_string(),
            use_container: true,
            is_editing_name: false,
            focused_pane: BrowserPane::FileSystem,
            preview_tree: vec![],
            preview_state: TreeViewState::default(),
            cursor_pos: 1,
            original_name_backup: "x".to_string(),
        };
        let state = TreeViewState::default();

        let out = reduce_browser_dialog_action(
            BrowserDialogAction::CancelDownloadSelection,
            &state,
            &mode,
            true,
        );

        assert_eq!(out.effects.len(), 3);
        assert!(matches!(
            out.effects[0],
            BrowserDialogEffect::CleanupPendingLink { async_delete: true }
        ));
        assert!(matches!(out.effects[1], BrowserDialogEffect::ClearSearch));
        assert!(matches!(
            out.effects[2],
            BrowserDialogEffect::ToNormalAndClearPending
        ));
    }

    #[test]
    fn pending_link_hash_is_none_for_empty() {
        assert!(pending_link_info_hash("").is_none());
    }

    #[test]
    fn cleanup_pending_link_is_noop_for_empty() {
        let mut txs: HashMap<Vec<u8>, Sender<ManagerCommand>> = HashMap::new();
        let mut torrents: HashMap<Vec<u8>, TorrentDisplayState> = HashMap::new();
        let mut order = vec![];
        cleanup_pending_link_on_escape("", &mut txs, &mut torrents, &mut order, false);
        assert!(txs.is_empty());
        assert!(torrents.is_empty());
        assert!(order.is_empty());
    }

    #[test]
    fn apply_priority_cycle_updates_target_tree() {
        let mut nodes = vec![RawNode {
            name: "root".to_string(),
            full_path: PathBuf::from("root"),
            children: vec![RawNode {
                name: "leaf".to_string(),
                full_path: PathBuf::from("root/leaf"),
                children: vec![],
                payload: TorrentPreviewPayload::default(),
                is_dir: false,
            }],
            payload: TorrentPreviewPayload::default(),
            is_dir: true,
        }];

        let changed = apply_priority_cycle(&mut nodes, &PathBuf::from("root"));
        assert!(changed);
        assert_eq!(nodes[0].payload.priority, FilePriority::Skip);
        assert_eq!(nodes[0].children[0].payload.priority, FilePriority::Skip);
    }
}
