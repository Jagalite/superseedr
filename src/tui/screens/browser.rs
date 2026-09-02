// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

#[cfg(test)]
use crate::app::App;
use crate::app::{
    refresh_torrent_preview_directory_priorities, AppMode, AppState, BrowserPane,
    BrowserSearchState, ConfigItem, ConfigUiState, DownloadSelectionTarget, FileBrowserMode,
    FileMetadata, FilePriority, SearchMode, TorrentFilePreview, TorrentFilePreviewState,
    TorrentPreviewPayload, AWAITING_MAGNET_METADATA_LABEL,
};
use crate::terminal_event::{Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind};
use crate::theme::ThemeContext;
use crate::tui::action_style::{footer_key_style, ActionTone};
use crate::tui::effects::{
    BrowserDialogEffect, BrowserFsEffect, ConfirmDecision, DownloadConfirmPayload,
};
use crate::tui::formatters::{centered_rect, format_bytes, sanitize_text, truncate_with_ellipsis};
use crate::tui::layout::browser::calculate_file_browser_layout;
use crate::tui::screen_context::ScreenContext;
use crate::tui::screens::input_panel::draw_prompt_panel;
use crate::tui::tree::{
    RawNode, TreeAction, TreeFilter, TreeMathHelper, TreeProjection, TreeViewState,
};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::prelude::{Alignment, Frame, Line, Modifier, Span, Style};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

const ASCII_TREE_DIR_ICON: &str = "> ";
const ASCII_TREE_FILE_ICON: &str = "  ";
const ASCII_TREE_ROOT_ICON: &str = "> ";

pub fn draw(
    f: &mut Frame,
    screen: &ScreenContext<'_>,
    state: &TreeViewState,
    data: &[RawNode<FileMetadata>],
    browser_mode: &FileBrowserMode,
) {
    let app_state = screen.ui;
    let ctx = screen.theme;

    let selected_filesystem_file = selected_filesystem_file_path(state, data);
    let has_preview_content = preview_content_for_selection(
        browser_mode,
        app_state.pending_torrent_path.is_some(),
        !app_state.pending_torrent_link.is_empty(),
        state,
        data,
    );
    let existing_priority_only = existing_torrent_priority_only(browser_mode);

    let preview_file_path = match browser_mode {
        FileBrowserMode::DownloadLocSelection { .. } => app_state.pending_torrent_path.as_ref(),
        FileBrowserMode::File(_) => selected_filesystem_file,
        _ => None,
    };

    let focused_pane = focused_pane(browser_mode);
    let search_panel_active = browser_search_panel_active(app_state.ui.file_browser.search_state);
    let max_area = centered_rect(90, 80, f.area());
    f.render_widget(Clear, max_area);

    let area = calculate_area(f.area(), has_preview_content);
    let layout = calculate_file_browser_layout(
        area,
        has_preview_content,
        search_panel_active,
        &focused_pane,
        existing_priority_only,
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
        let preview_filter = if matches!(focused_pane, BrowserPane::TorrentPreview) {
            build_torrent_preview_filter(
                &app_state.ui.file_browser.search_query,
                app_state.ui.file_browser.search_mode,
            )
        } else {
            TreeFilter::default()
        };
        draw_torrent_preview_panel(
            f,
            ctx,
            preview_area,
            TorrentPreviewPanelProps {
                path: preview_file_path.map(|p| p.as_path()),
                browser_mode,
                border_style: preview_border_style,
                current_fs_path: &state.current_path,
                preview_filter,
                torrent_file_preview: &app_state.ui.file_browser.torrent_file_preview,
            },
        );
    }
    if let Some(search_area) = layout.search {
        draw_browser_search_panel(
            f,
            app_state.ui.file_browser.search_mode,
            &focused_pane,
            &app_state.ui.file_browser.search_query,
            search_area,
            ctx,
        );
    }

    let mut footer_spans = Vec::new();
    let editing_container_name = browser_container_name_editing(browser_mode);
    if editing_container_name {
        footer_spans.push(Span::styled(
            "[Enter]",
            footer_key_style(ctx, ActionTone::Confirm),
        ));
        footer_spans.push(Span::raw(" Save Folder Name | "));
        footer_spans.push(Span::styled(
            "[Esc]",
            footer_key_style(ctx, ActionTone::Cancel),
        ));
        footer_spans.push(Span::raw(" Discard Changes"));
    } else {
        match browser_mode {
            FileBrowserMode::ConfigPathSelection { .. } | FileBrowserMode::Directory => {
                footer_spans.push(Span::styled(
                    "[Arrows/Vim]",
                    footer_key_style(ctx, ActionTone::Navigate),
                ));
                footer_spans.push(Span::raw(" Nav | "));
                footer_spans.push(Span::styled(
                    "[Backspace]",
                    footer_key_style(ctx, ActionTone::Navigate),
                ));
                footer_spans.push(Span::raw(" Up | "));
                footer_spans.push(Span::styled(
                    "[Enter]",
                    footer_key_style(ctx, ActionTone::Navigate),
                ));
                footer_spans.push(Span::raw(" Down | "));
                footer_spans.push(Span::styled(
                    "[Y]",
                    footer_key_style(ctx, ActionTone::Confirm),
                ));
                footer_spans.push(Span::raw(" Confirm Selection | "));
            }
            FileBrowserMode::DownloadLocSelection {
                focused_pane,
                use_container,
                ..
            } => {
                let edit_locked = pending_magnet_metadata_editing_locked(browser_mode);
                if !existing_priority_only {
                    footer_spans.push(Span::styled(
                        "[Tab]",
                        footer_key_style(ctx, ActionTone::Mode),
                    ));
                    if search_panel_active {
                        footer_spans.push(Span::raw(" Search Mode | "));
                    } else {
                        footer_spans.push(Span::raw(" Switch Pane | "));
                    }
                }
                footer_spans.push(Span::styled("[/]", footer_key_style(ctx, ActionTone::Edit)));
                footer_spans.push(Span::raw(" Search | "));

                if existing_priority_only || matches!(focused_pane, BrowserPane::TorrentPreview) {
                    footer_spans.push(Span::styled(
                        "[Space/p]",
                        footer_key_style(ctx, ActionTone::Toggle),
                    ));
                    footer_spans.push(Span::raw(" Priority | "));
                    footer_spans.push(Span::styled(
                        "[P]",
                        footer_key_style(ctx, ActionTone::Toggle),
                    ));
                    footer_spans.push(Span::raw(" Priority All | "));
                    footer_spans.push(Span::styled(
                        "[e]",
                        footer_key_style(ctx, ActionTone::Navigate),
                    ));
                    footer_spans.push(Span::raw(" Expand | "));
                    footer_spans.push(Span::styled(
                        "[c]",
                        footer_key_style(ctx, ActionTone::Navigate),
                    ));
                    footer_spans.push(Span::raw(" Collapse | "));
                }

                if !existing_priority_only && !edit_locked {
                    footer_spans.push(Span::styled(
                        "[x]",
                        footer_key_style(ctx, ActionTone::Toggle),
                    ));
                    footer_spans.push(Span::raw(" Container Folder | "));

                    if *use_container {
                        footer_spans
                            .push(Span::styled("[r]", footer_key_style(ctx, ActionTone::Edit)));
                        footer_spans.push(Span::raw(" Rename | "));
                    }
                }

                footer_spans.push(Span::styled(
                    "[Y]",
                    footer_key_style(ctx, ActionTone::Confirm),
                ));
                footer_spans.push(Span::raw(" Confirm"));
            }
            FileBrowserMode::File(_) => {
                footer_spans.push(Span::styled(
                    "[Y]",
                    footer_key_style(ctx, ActionTone::Confirm),
                ));
                footer_spans.push(Span::raw(" Confirm File"));
            }
        }
        footer_spans.push(Span::raw(" | "));
        footer_spans.push(Span::styled(
            "[Esc]",
            footer_key_style(ctx, ActionTone::Cancel),
        ));
        footer_spans.push(Span::raw(" Cancel"));
    }

    let footer = Paragraph::new(Line::from(footer_spans))
        .alignment(Alignment::Center)
        .style(ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)));
    f.render_widget(footer, layout.footer);

    if existing_priority_only {
        return;
    }

    let inner_height = layout.list.height.saturating_sub(2) as usize;
    let list_width = layout.list.width.saturating_sub(2) as usize;
    let filesystem_search_query = if matches!(focused_pane, BrowserPane::FileSystem) {
        app_state.ui.file_browser.search_query.as_str()
    } else {
        ""
    };
    let filter = build_filesystem_filter(
        browser_mode,
        filesystem_search_query,
        app_state.ui.file_browser.search_mode,
    );

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

    if let Some(error) = app_state.ui.file_browser.fetch_error.as_deref() {
        list_items.push(ListItem::new(Line::from(vec![Span::styled(
            file_browser_fetch_error_message(error),
            ctx.apply(Style::default().fg(ctx.state_error())).bold(),
        )])));
    } else if data.is_empty() {
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

fn existing_torrent_priority_only(browser_mode: &FileBrowserMode) -> bool {
    matches!(
        browser_mode,
        FileBrowserMode::DownloadLocSelection {
            target: DownloadSelectionTarget::ExistingTorrent { .. },
            ..
        }
    )
}

fn browser_search_panel_active(search_state: BrowserSearchState) -> bool {
    search_state.is_visible()
}

fn draw_browser_search_panel(
    f: &mut Frame,
    search_mode: SearchMode,
    focused_pane: &BrowserPane,
    search_query: &str,
    area: Rect,
    ctx: &ThemeContext,
) {
    let title = match focused_pane {
        BrowserPane::TorrentPreview => " Torrent File Search ".to_string(),
        BrowserPane::FileSystem => " File Browser Search ".to_string(),
    };
    draw_prompt_panel(
        f,
        area,
        title,
        sanitize_text(search_query),
        browser_search_mode_spans(search_mode, ctx),
        ctx,
    );
}

fn browser_search_mode_spans(search_mode: SearchMode, ctx: &ThemeContext) -> Vec<Span<'static>> {
    let (fuzzy_style, regex_style) = match search_mode {
        SearchMode::Fuzzy => (
            ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
        ),
        SearchMode::Regex => (
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
            ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
        ),
    };
    vec![
        Span::raw("  "),
        Span::styled("Fuzzy", fuzzy_style),
        Span::raw(" / "),
        Span::styled("Regex", regex_style),
        Span::raw("  Tab"),
    ]
}

struct TorrentPreviewPanelProps<'a> {
    path: Option<&'a Path>,
    browser_mode: &'a FileBrowserMode,
    border_style: Style,
    current_fs_path: &'a Path,
    preview_filter: TreeFilter<TorrentPreviewPayload>,
    torrent_file_preview: &'a TorrentFilePreviewState,
}

fn draw_torrent_preview_panel(
    f: &mut Frame,
    ctx: &ThemeContext,
    area: Rect,
    props: TorrentPreviewPanelProps<'_>,
) {
    let TorrentPreviewPanelProps {
        path,
        browser_mode,
        border_style,
        current_fs_path,
        preview_filter,
        torrent_file_preview,
    } = props;
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
        let priority_only = existing_torrent_priority_only(browser_mode);
        let header_lines = if *use_container && !priority_only {
            2
        } else {
            1
        };
        let list_height = inner_area.height.saturating_sub(header_lines) as usize;

        let visible_rows = TreeMathHelper::get_visible_slice(
            preview_tree,
            preview_state,
            preview_filter,
            list_height,
        );

        let mut list_items = Vec::new();
        let root_style = Style::default()
            .fg(ctx.state_info())
            .add_modifier(Modifier::BOLD);

        let path_display = if priority_only {
            "Priority editor (location unchanged)".to_string()
        } else if is_narrow {
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

        if *use_container && !priority_only {
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
                let split_pos = clamp_to_char_boundary(container_name, *cursor_pos);
                let (before, after) = container_name.split_at(split_pos);
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
                let base_indent_level = if *use_container && !priority_only {
                    2
                } else {
                    1
                };
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
                    Span::styled(sanitize_text(&item.node.name), final_content_style),
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

    if let Some(path) = path {
        draw_cached_torrent_file_preview(f, ctx, inner_area, is_narrow, path, torrent_file_preview);
    }
}

fn draw_cached_torrent_file_preview(
    f: &mut Frame,
    ctx: &ThemeContext,
    area: Rect,
    is_narrow: bool,
    selected_path: &Path,
    state: &TorrentFilePreviewState,
) {
    match state {
        TorrentFilePreviewState::Ready { path, preview } if path == selected_path => {
            draw_loaded_torrent_file_preview(f, ctx, area, is_narrow, preview);
        }
        TorrentFilePreviewState::Error { path, message } if path == selected_path => {
            f.render_widget(
                Paragraph::new(format!("Preview Error: {}", sanitize_text(message)))
                    .style(ctx.apply(Style::default().fg(ctx.state_error()))),
                area,
            );
        }
        TorrentFilePreviewState::Loading { path, .. } if path == selected_path => {
            f.render_widget(
                Paragraph::new("Loading torrent preview...")
                    .style(ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0))),
                area,
            );
        }
        _ => {
            f.render_widget(
                Paragraph::new("Preparing torrent preview...")
                    .style(ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0))),
                area,
            );
        }
    }
}

fn draw_loaded_torrent_file_preview(
    f: &mut Frame,
    ctx: &ThemeContext,
    area: Rect,
    is_narrow: bool,
    preview: &TorrentFilePreview,
) {
    let info_text = vec![
        Line::from(vec![
            Span::styled(
                "Name: ",
                ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
            ),
            Span::raw(sanitize_text(&preview.name)),
        ]),
        Line::from(vec![
            Span::styled(
                "Protocol: ",
                ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
            ),
            Span::styled(
                preview.protocol_version.as_str(),
                Style::default().fg(ctx.state_selected()).bold(),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                "Size: ",
                ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
            ),
            Span::raw(format_bytes(preview.total_size)),
        ]),
    ];

    let layout = Layout::vertical([
        Constraint::Length(info_text.len() as u16 + 1),
        Constraint::Min(0),
    ])
    .split(area);
    f.render_widget(
        Paragraph::new(info_text).block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.border))),
        ),
        layout[0],
    );

    let mut expanded_state = TreeViewState::default();
    for node in &preview.tree {
        node.expand_all(&mut expanded_state);
    }
    let visible_rows = TreeMathHelper::get_visible_slice(
        &preview.tree,
        &expanded_state,
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
                Span::styled(sanitize_text(&item.node.name), style),
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

pub fn handle_event(event: CrosstermEvent, state: &mut AppState) -> BrowserHandleResult {
    if !matches!(state.mode, AppMode::FileBrowser) {
        return BrowserHandleResult::default();
    }

    if let CrosstermEvent::Key(key) = event {
        if key.kind == KeyEventKind::Press {
            // Container-name editing is modal. In particular, Esc must restore the
            // backup even when an applied search is still visible, and printable
            // shortcut keys belong to the name until the edit is committed.
            if browser_container_name_editing(&state.ui.file_browser.browser_mode) {
                return handle_browser_download_key(key.code, state);
            }

            if handle_browser_search_key(key, state) {
                return BrowserHandleResult::consumed();
            }

            let download_result = handle_browser_download_key(key.code, state);
            if download_result.consumed {
                return download_result;
            }

            return handle_browser_common_key(key.code, state);
        }
    }

    BrowserHandleResult::default()
}

fn handle_browser_search_key(key: KeyEvent, state: &mut AppState) -> bool {
    let pending_torrent_path = state.pending_torrent_path.is_some();
    let pending_torrent_link = !state.pending_torrent_link.is_empty();
    let screen_area = state.screen_area;
    let file_browser = &mut state.ui.file_browser;
    let has_preview = preview_content_for_selection(
        &file_browser.browser_mode,
        pending_torrent_path,
        pending_torrent_link,
        &file_browser.state,
        &file_browser.data,
    );
    if matches!(key.code, KeyCode::Tab) && browser_search_panel_active(file_browser.search_state) {
        toggle_browser_search_mode(&mut file_browser.search_mode);
        reset_active_browser_search_view(BrowserSearchViewContext {
            browser_mode: &mut file_browser.browser_mode,
            filesystem_state: &mut file_browser.state,
            filesystem_data: &file_browser.data,
            search_query: &file_browser.search_query,
            search_mode: file_browser.search_mode,
            screen_area,
            has_preview,
            search_panel_active: browser_search_panel_active(file_browser.search_state),
        });
        state.ui.needs_redraw = true;
        return true;
    }

    if let Some(action) = map_search_key_to_browser_action(key, file_browser.search_state) {
        let reset_view = browser_action_resets_search_view(action);
        let reduced = reduce_browser_action(
            action,
            &mut file_browser.search_state,
            &mut file_browser.search_query,
            &mut file_browser.search_mode,
        );
        if reset_view {
            reset_active_browser_search_view(BrowserSearchViewContext {
                browser_mode: &mut file_browser.browser_mode,
                filesystem_state: &mut file_browser.state,
                filesystem_data: &file_browser.data,
                search_query: &file_browser.search_query,
                search_mode: file_browser.search_mode,
                screen_area,
                has_preview,
                search_panel_active: browser_search_panel_active(file_browser.search_state),
            });
        }
        if reduced.redraw {
            state.ui.needs_redraw = true;
        }
        return true;
    }
    false
}

fn handle_browser_download_key(key_code: KeyCode, state: &mut AppState) -> BrowserHandleResult {
    let consumed_download_input = {
        let browser_mode = &mut state.ui.file_browser.browser_mode;
        if let Some(action) = map_download_key_to_action(key_code, browser_mode) {
            reduce_browser_download_action(action, browser_mode).consumed
        } else {
            false
        }
    };
    if consumed_download_input {
        return BrowserHandleResult::consumed();
    }

    if !matches!(
        state.ui.file_browser.browser_mode,
        FileBrowserMode::DownloadLocSelection { .. }
    ) {
        return BrowserHandleResult::default();
    }

    if preview_search_should_start(key_code, &state.ui.file_browser.browser_mode) {
        let pending_torrent_path = state.pending_torrent_path.is_some();
        let pending_torrent_link = !state.pending_torrent_link.is_empty();
        let screen_area = state.screen_area;
        let file_browser = &mut state.ui.file_browser;
        start_browser_search(
            &mut file_browser.search_state,
            &mut file_browser.search_query,
            &mut file_browser.search_mode,
        );
        let has_preview = preview_content_for_selection(
            &file_browser.browser_mode,
            pending_torrent_path,
            pending_torrent_link,
            &file_browser.state,
            &file_browser.data,
        );
        reset_active_browser_search_view(BrowserSearchViewContext {
            browser_mode: &mut file_browser.browser_mode,
            filesystem_state: &mut file_browser.state,
            filesystem_data: &file_browser.data,
            search_query: &file_browser.search_query,
            search_mode: file_browser.search_mode,
            screen_area,
            has_preview,
            search_panel_active: browser_search_panel_active(file_browser.search_state),
        });
        state.ui.needs_redraw = true;
        return BrowserHandleResult::consumed();
    }

    if key_code == KeyCode::Esc {
        let reduced = {
            let file_browser = &state.ui.file_browser;
            reduce_browser_dialog_action(
                BrowserDialogAction::CancelDownloadSelection,
                &file_browser.state,
                &file_browser.data,
                &file_browser.browser_mode,
                !state.pending_torrent_link.is_empty(),
            )
        };
        return BrowserHandleResult {
            consumed: true,
            fs_effects: Vec::new(),
            dialog_effects: reduced.effects,
        };
    }

    let screen_area = state.screen_area;
    let search_state = state.ui.file_browser.search_state;
    let search_query = state.ui.file_browser.search_query.clone();
    let search_panel_active = browser_search_panel_active(search_state);
    let search_mode = state.ui.file_browser.search_mode;
    let consumed_preview_input = {
        let browser_mode = &mut state.ui.file_browser.browser_mode;
        if let FileBrowserMode::DownloadLocSelection {
            use_container,
            focused_pane,
            preview_tree,
            preview_state,
            target,
            ..
        } = browser_mode
        {
            if matches!(focused_pane, BrowserPane::TorrentPreview) {
                let preview_only =
                    matches!(target, DownloadSelectionTarget::ExistingTorrent { .. });
                let list_height = calculate_preview_list_height(
                    screen_area,
                    search_panel_active,
                    focused_pane,
                    *use_container,
                    preview_only,
                );
                reduce_browser_preview_action(
                    map_preview_key_to_action(key_code),
                    preview_state,
                    preview_tree,
                    build_torrent_preview_filter(&search_query, search_mode),
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
        return BrowserHandleResult::consumed();
    }

    BrowserHandleResult::default()
}

fn preview_search_should_start(key_code: KeyCode, browser_mode: &FileBrowserMode) -> bool {
    matches!(key_code, KeyCode::Char('/'))
        && matches!(
            browser_mode,
            FileBrowserMode::DownloadLocSelection {
                focused_pane: BrowserPane::TorrentPreview,
                is_editing_name: false,
                ..
            }
        )
}

fn start_browser_search(
    search_state: &mut BrowserSearchState,
    search_query: &mut String,
    search_mode: &mut SearchMode,
) {
    *search_state = BrowserSearchState::Editing;
    search_query.clear();
    *search_mode = SearchMode::Regex;
}

fn clear_browser_search(search_state: &mut BrowserSearchState, search_query: &mut String) {
    *search_state = BrowserSearchState::Closed;
    search_query.clear();
}

fn toggle_browser_search_mode(search_mode: &mut SearchMode) {
    *search_mode = match *search_mode {
        SearchMode::Fuzzy => SearchMode::Regex,
        SearchMode::Regex => SearchMode::Fuzzy,
    };
}

fn browser_action_resets_search_view(action: BrowserAction) -> bool {
    matches!(
        action,
        BrowserAction::Esc
            | BrowserAction::Backspace
            | BrowserAction::ToggleSearchMode
            | BrowserAction::Char(_)
    )
}

fn cursor_is_in_visible_window<T>(
    projection: &TreeProjection<'_, T>,
    state: &TreeViewState,
) -> bool {
    let Some(cursor_path) = state.cursor_path.as_ref() else {
        return false;
    };
    projection
        .visible_window()
        .iter()
        .any(|item| &item.path == cursor_path)
}

fn reconcile_tree_cursor_with_visible_window<T>(
    state: &mut TreeViewState,
    data: &[RawNode<T>],
    filter: TreeFilter<T>,
    list_height: usize,
) {
    state.top_most_offset = 0;
    let projection = TreeProjection::new(data, state, filter, list_height);
    if !cursor_is_in_visible_window(&projection, state) {
        state.cursor_path = projection
            .visible_window()
            .first()
            .map(|item| item.path.clone());
    }
}

struct BrowserSearchViewContext<'a> {
    browser_mode: &'a mut FileBrowserMode,
    filesystem_state: &'a mut TreeViewState,
    filesystem_data: &'a [RawNode<FileMetadata>],
    search_query: &'a str,
    search_mode: SearchMode,
    screen_area: Rect,
    has_preview: bool,
    search_panel_active: bool,
}

fn reset_active_browser_search_view(ctx: BrowserSearchViewContext<'_>) {
    let BrowserSearchViewContext {
        browser_mode,
        filesystem_state,
        filesystem_data,
        search_query,
        search_mode,
        screen_area,
        has_preview,
        search_panel_active,
    } = ctx;
    if let FileBrowserMode::DownloadLocSelection {
        target,
        use_container,
        focused_pane: BrowserPane::TorrentPreview,
        preview_tree,
        preview_state,
        ..
    } = browser_mode
    {
        let preview_only = matches!(target, DownloadSelectionTarget::ExistingTorrent { .. });
        if let Some(list_height) = calculate_preview_list_height(
            screen_area,
            search_panel_active,
            &BrowserPane::TorrentPreview,
            *use_container,
            preview_only,
        ) {
            reconcile_tree_cursor_with_visible_window(
                preview_state,
                preview_tree,
                build_torrent_preview_filter(search_query, search_mode),
                list_height,
            );
        } else {
            preview_state.top_most_offset = 0;
            preview_state.cursor_path = None;
        }
        return;
    }

    let pane = focused_pane(browser_mode);
    let list_height = calculate_list_height(screen_area, has_preview, search_panel_active, &pane);
    reconcile_tree_cursor_with_visible_window(
        filesystem_state,
        filesystem_data,
        build_filesystem_filter(browser_mode, search_query, search_mode),
        list_height,
    );
}

fn handle_browser_common_key(key_code: KeyCode, state: &mut AppState) -> BrowserHandleResult {
    let list_height = {
        let file_browser = &state.ui.file_browser;
        let has_preview = preview_content_for_selection(
            &file_browser.browser_mode,
            state.pending_torrent_path.is_some(),
            !state.pending_torrent_link.is_empty(),
            &file_browser.state,
            &file_browser.data,
        );
        let pane = focused_pane(&file_browser.browser_mode);
        let search_panel_active = browser_search_panel_active(file_browser.search_state);
        calculate_list_height(state.screen_area, has_preview, search_panel_active, &pane)
    };

    let reduced_filesystem = {
        let file_browser = &mut state.ui.file_browser;
        handle_filesystem_navigation(
            key_code,
            BrowserFilesystemNavContext {
                state: &mut file_browser.state,
                data: &file_browser.data,
                browser_mode: &file_browser.browser_mode,
                search_state: &mut file_browser.search_state,
                search_query: &mut file_browser.search_query,
                search_mode: &mut file_browser.search_mode,
                list_height,
            },
        )
    };
    if reduced_filesystem.consumed {
        return BrowserHandleResult {
            consumed: true,
            fs_effects: reduced_filesystem.effects,
            dialog_effects: Vec::new(),
        };
    }

    let dialog_action = match key_code {
        KeyCode::Char('Y') => Some(BrowserDialogAction::ConfirmSelection),
        KeyCode::Esc => Some(BrowserDialogAction::Escape),
        _ => None,
    };
    let Some(dialog_action) = dialog_action else {
        return BrowserHandleResult::default();
    };

    let reduced = {
        let file_browser = &state.ui.file_browser;
        if matches!(dialog_action, BrowserDialogAction::ConfirmSelection) {
            if file_browser.fetch_pending {
                return BrowserHandleResult::consumed();
            }
            if matches!(file_browser.browser_mode, FileBrowserMode::File(_))
                && !filesystem_cursor_visible(
                    &file_browser.data,
                    &file_browser.state,
                    &file_browser.browser_mode,
                    &file_browser.search_query,
                    file_browser.search_mode,
                    list_height,
                )
            {
                return BrowserHandleResult::consumed();
            }
        }
        reduce_browser_dialog_action(
            dialog_action,
            &file_browser.state,
            &file_browser.data,
            &file_browser.browser_mode,
            !state.pending_torrent_link.is_empty(),
        )
    };
    BrowserHandleResult {
        consumed: true,
        fs_effects: Vec::new(),
        dialog_effects: reduced.effects,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BrowserAction {
    Esc,
    Enter,
    Backspace,
    ToggleSearchMode,
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

pub struct BrowserDialogReduceResult {
    pub effects: Vec<BrowserDialogEffect>,
}

#[derive(Default)]
pub struct BrowserHandleResult {
    pub consumed: bool,
    pub fs_effects: Vec<BrowserFsEffect>,
    pub dialog_effects: Vec<BrowserDialogEffect>,
}

impl BrowserHandleResult {
    fn consumed() -> Self {
        Self {
            consumed: true,
            ..Self::default()
        }
    }
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
    CycleAllPriorities,
    ExpandAll,
    CollapseAll,
    Ignore,
}

pub struct BrowserPreviewReduceResult {
    pub consumed: bool,
}

pub struct BrowserFilesystemNavContext<'a> {
    pub state: &'a mut TreeViewState,
    pub data: &'a [RawNode<FileMetadata>],
    pub browser_mode: &'a FileBrowserMode,
    pub search_state: &'a mut BrowserSearchState,
    pub search_query: &'a mut String,
    pub search_mode: &'a mut SearchMode,
    pub list_height: usize,
}

pub struct BrowserFilesystemReduceContext<'a> {
    pub state: &'a mut TreeViewState,
    pub data: &'a [RawNode<FileMetadata>],
    pub browser_mode: &'a FileBrowserMode,
    pub search_state: &'a mut BrowserSearchState,
    pub search_query: &'a mut String,
    pub search_mode: &'a mut SearchMode,
    pub list_height: usize,
}

fn map_search_key_to_browser_action(
    key: KeyEvent,
    search_state: BrowserSearchState,
) -> Option<BrowserAction> {
    if !search_state.is_editing() {
        return if search_state.is_visible() && matches!(key.code, KeyCode::Esc) {
            Some(BrowserAction::Esc)
        } else {
            None
        };
    }

    Some(match key.code {
        KeyCode::Esc => BrowserAction::Esc,
        KeyCode::Enter => BrowserAction::Enter,
        KeyCode::Tab => BrowserAction::ToggleSearchMode,
        KeyCode::Backspace => BrowserAction::Backspace,
        KeyCode::Char(c) => BrowserAction::Char(c),
        _ => BrowserAction::Noop,
    })
}

pub fn reduce_browser_action(
    action: BrowserAction,
    search_state: &mut BrowserSearchState,
    search_query: &mut String,
    search_mode: &mut SearchMode,
) -> BrowserReduceResult {
    match action {
        BrowserAction::Esc => {
            clear_browser_search(search_state, search_query);
        }
        BrowserAction::Enter => {
            *search_state = if search_query.is_empty() {
                BrowserSearchState::Closed
            } else {
                BrowserSearchState::Applied
            };
        }
        BrowserAction::Backspace => {
            search_query.pop();
        }
        BrowserAction::ToggleSearchMode => {
            toggle_browser_search_mode(search_mode);
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
    ctx: BrowserFilesystemReduceContext<'_>,
) -> BrowserFsReduceResult {
    let mut result = BrowserFsReduceResult {
        consumed: true,
        effects: Vec::new(),
    };

    match action {
        BrowserFsAction::StartSearch => {
            start_browser_search(ctx.search_state, ctx.search_query, ctx.search_mode);
            reconcile_tree_cursor_with_visible_window(
                ctx.state,
                ctx.data,
                build_filesystem_filter(ctx.browser_mode, ctx.search_query, *ctx.search_mode),
                ctx.list_height,
            );
        }
        BrowserFsAction::Move(tree_action) => {
            TreeMathHelper::apply_action(
                ctx.state,
                ctx.data,
                tree_action,
                build_filesystem_filter(ctx.browser_mode, ctx.search_query, *ctx.search_mode),
                ctx.list_height,
            );
        }
        BrowserFsAction::EnterDir => {
            let projection = TreeProjection::new(
                ctx.data,
                ctx.state,
                build_filesystem_filter(ctx.browser_mode, ctx.search_query, *ctx.search_mode),
                ctx.list_height,
            );
            let selected_directory = ctx.state.cursor_path.as_ref().and_then(|cursor_path| {
                projection
                    .visible_window()
                    .iter()
                    .find(|item| &item.path == cursor_path && item.node.is_dir)
                    .map(|item| item.path.clone())
            });
            if let Some(path) = selected_directory {
                clear_browser_search(ctx.search_state, ctx.search_query);
                result.effects.push(BrowserFsEffect::FetchFileTree {
                    path,
                    browser_mode: ctx.browser_mode.clone(),
                    highlight_path: None,
                });
            }
        }
        BrowserFsAction::GoParent => {
            let child_to_highlight = ctx.state.current_path.clone();
            if let Some(parent) = ctx.state.current_path.parent() {
                result.effects.push(BrowserFsEffect::FetchFileTree {
                    path: parent.to_path_buf(),
                    browser_mode: ctx.browser_mode.clone(),
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

fn browser_container_name_editing(browser_mode: &FileBrowserMode) -> bool {
    matches!(
        browser_mode,
        FileBrowserMode::DownloadLocSelection {
            is_editing_name: true,
            ..
        }
    )
}

fn pending_magnet_metadata_editing_locked(browser_mode: &FileBrowserMode) -> bool {
    matches!(
        browser_mode,
        FileBrowserMode::DownloadLocSelection {
            target: DownloadSelectionTarget::PendingAdd,
            preview_tree,
            container_name,
            ..
        } if preview_tree.is_empty() && container_name == AWAITING_MAGNET_METADATA_LABEL
    )
}

pub fn map_download_key_to_action(
    key_code: KeyCode,
    browser_mode: &FileBrowserMode,
) -> Option<BrowserDownloadAction> {
    let edit_locked = pending_magnet_metadata_editing_locked(browser_mode);
    if let FileBrowserMode::DownloadLocSelection {
        is_editing_name,
        use_container,
        target,
        ..
    } = browser_mode
    {
        if edit_locked {
            if key_code == KeyCode::Tab {
                return Some(BrowserDownloadAction::Shortcut(
                    BrowserDownloadShortcutAction::TogglePane,
                ));
            }
            return None;
        }

        if *is_editing_name {
            return Some(BrowserDownloadAction::Edit(
                map_download_name_edit_key_to_action(key_code),
            ));
        }

        if matches!(target, DownloadSelectionTarget::ExistingTorrent { .. }) {
            return None;
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
    *cursor_pos = clamp_to_char_boundary(container_name, *cursor_pos);

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
            *cursor_pos = previous_char_boundary(container_name, *cursor_pos);
        }
        BrowserDownloadEditAction::MoveRight => {
            *cursor_pos = next_char_boundary(container_name, *cursor_pos);
        }
        BrowserDownloadEditAction::Backspace => {
            if *cursor_pos > 0 {
                let previous = previous_char_boundary(container_name, *cursor_pos);
                container_name.drain(previous..*cursor_pos);
                *cursor_pos = previous;
            }
        }
        BrowserDownloadEditAction::Delete => {
            if *cursor_pos < container_name.len() {
                let next = next_char_boundary(container_name, *cursor_pos);
                container_name.drain(*cursor_pos..next);
            }
        }
        BrowserDownloadEditAction::Insert(c) => {
            container_name.insert(*cursor_pos, c);
            *cursor_pos += c.len_utf8();
        }
        BrowserDownloadEditAction::Noop => {}
    }

    BrowserDownloadEditReduceResult { consumed: true }
}

fn clamp_to_char_boundary(value: &str, cursor_pos: usize) -> usize {
    let mut pos = cursor_pos.min(value.len());
    while pos > 0 && !value.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

fn previous_char_boundary(value: &str, cursor_pos: usize) -> usize {
    let pos = clamp_to_char_boundary(value, cursor_pos);
    if pos == 0 {
        return 0;
    }
    value[..pos]
        .char_indices()
        .last()
        .map(|(idx, _)| idx)
        .unwrap_or(0)
}

fn next_char_boundary(value: &str, cursor_pos: usize) -> usize {
    let pos = clamp_to_char_boundary(value, cursor_pos);
    if pos >= value.len() {
        return value.len();
    }
    value[pos..]
        .char_indices()
        .nth(1)
        .map(|(idx, _)| pos + idx)
        .unwrap_or(value.len())
}

fn filesystem_cursor_visible(
    data: &[RawNode<FileMetadata>],
    state: &TreeViewState,
    browser_mode: &FileBrowserMode,
    search_query: &str,
    search_mode: SearchMode,
    list_height: usize,
) -> bool {
    let filter = build_filesystem_filter(browser_mode, search_query, search_mode);
    let projection = TreeProjection::new(data, state, filter, list_height);
    cursor_is_in_visible_window(&projection, state)
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
    let edit_locked = pending_magnet_metadata_editing_locked(browser_mode);
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
        if edit_locked
            && !matches!(
                action,
                BrowserDownloadAction::Shortcut(BrowserDownloadShortcutAction::TogglePane)
            )
        {
            return BrowserDownloadReduceResult { consumed: false };
        }

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
        FileBrowserMode::DownloadLocSelection {
            target,
            preview_tree,
            ..
        } => {
            pending_torrent_path
                || pending_torrent_link
                || !preview_tree.is_empty()
                || matches!(target, DownloadSelectionTarget::ExistingTorrent { .. })
        }
        FileBrowserMode::File(_) => cursor_path.is_some_and(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("torrent"))
        }),
        _ => false,
    }
}

pub fn selected_filesystem_file_path<'a>(
    state: &TreeViewState,
    data: &'a [RawNode<FileMetadata>],
) -> Option<&'a PathBuf> {
    let cursor_path = state.cursor_path.as_ref()?;
    data.iter()
        .find(|node| !node.is_dir && &node.full_path == cursor_path)
        .map(|node| &node.full_path)
}

pub fn preview_content_for_selection(
    browser_mode: &FileBrowserMode,
    pending_torrent_path: bool,
    pending_torrent_link: bool,
    state: &TreeViewState,
    data: &[RawNode<FileMetadata>],
) -> bool {
    has_preview_content(
        browser_mode,
        pending_torrent_path,
        pending_torrent_link,
        selected_filesystem_file_path(state, data),
    )
}

fn file_browser_fetch_error_message(error: &str) -> String {
    format!("   Error: {}", sanitize_text(error))
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
        calculate_file_browser_layout(area, has_preview_content, is_searching, focused_pane, false);
    layout.list.height.saturating_sub(2) as usize
}

pub fn calculate_preview_list_height(
    screen: Rect,
    is_searching: bool,
    focused_pane: &BrowserPane,
    use_container: bool,
    preview_only: bool,
) -> Option<usize> {
    let area = if screen.width < 60 {
        screen
    } else {
        centered_rect(90, 80, screen)
    };
    let layout =
        calculate_file_browser_layout(area, true, is_searching, focused_pane, preview_only);
    layout.preview.map(|preview_rect| {
        let inner_height = preview_rect.height.saturating_sub(2);
        let header_rows = if use_container && !preview_only { 2 } else { 1 };
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
        KeyCode::Char('P') => BrowserPreviewAction::CycleAllPriorities,
        KeyCode::Char(' ') | KeyCode::Char('p') => BrowserPreviewAction::CyclePriority,
        KeyCode::Char('e') => BrowserPreviewAction::ExpandAll,
        KeyCode::Char('c') => BrowserPreviewAction::CollapseAll,
        _ => BrowserPreviewAction::Ignore,
    }
}

pub fn reduce_browser_preview_action(
    action: BrowserPreviewAction,
    preview_state: &mut TreeViewState,
    preview_tree: &mut [RawNode<TorrentPreviewPayload>],
    filter: TreeFilter<TorrentPreviewPayload>,
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
                    filter,
                    height,
                );
            }
            BrowserPreviewReduceResult { consumed: true }
        }
        BrowserPreviewAction::CyclePriority => {
            if let Some(height) = list_height {
                let cursor_visible = {
                    let projection =
                        TreeProjection::new(preview_tree, preview_state, filter, height);
                    cursor_is_in_visible_window(&projection, preview_state)
                };
                if cursor_visible {
                    if let Some(target) = &preview_state.cursor_path {
                        apply_priority_cycle(preview_tree, target);
                    }
                }
            }
            BrowserPreviewReduceResult { consumed: true }
        }
        BrowserPreviewAction::CycleAllPriorities => {
            if let Some(_height) = list_height {
                apply_priority_cycle_to_all(preview_tree);
            }
            BrowserPreviewReduceResult { consumed: true }
        }
        BrowserPreviewAction::ExpandAll => {
            expand_preview_tree(preview_state, preview_tree);
            BrowserPreviewReduceResult { consumed: true }
        }
        BrowserPreviewAction::CollapseAll => {
            collapse_preview_tree(preview_state, preview_tree);
            BrowserPreviewReduceResult { consumed: true }
        }
        BrowserPreviewAction::Ignore => BrowserPreviewReduceResult { consumed: true },
    }
}

pub fn build_filesystem_filter(
    browser_mode: &FileBrowserMode,
    search_query: &str,
    search_mode: SearchMode,
) -> TreeFilter<FileMetadata> {
    if search_query.trim().is_empty() {
        return match browser_mode {
            FileBrowserMode::File(extensions) => {
                let exts = extensions.clone();
                TreeFilter::new("", move |node| filesystem_node_allowed(node, &exts))
            }
            FileBrowserMode::Directory
            | FileBrowserMode::DownloadLocSelection { .. }
            | FileBrowserMode::ConfigPathSelection { .. } => TreeFilter::default(),
        };
    }

    let extensions = match browser_mode {
        FileBrowserMode::File(extensions) => Some(extensions.clone()),
        FileBrowserMode::Directory
        | FileBrowserMode::DownloadLocSelection { .. }
        | FileBrowserMode::ConfigPathSelection { .. } => None,
    };

    match search_mode {
        SearchMode::Fuzzy => {
            let matcher = SkimMatcherV2::default();
            let query = search_query.to_lowercase();
            TreeFilter::rule_only(search_query, move |node| {
                filesystem_node_allowed_for_mode(node, extensions.as_deref())
                    && matcher
                        .fuzzy_match(&filesystem_search_haystack(node).to_lowercase(), &query)
                        .is_some()
            })
        }
        SearchMode::Regex => {
            let regex = regex::RegexBuilder::new(search_query)
                .case_insensitive(true)
                .build();
            match regex {
                Ok(regex) => TreeFilter::rule_only(search_query, move |node| {
                    filesystem_node_allowed_for_mode(node, extensions.as_deref())
                        && regex.is_match(&filesystem_search_haystack(node))
                }),
                Err(_) => TreeFilter::rule_only(search_query, |_| false),
            }
        }
    }
}

fn filesystem_node_allowed_for_mode(
    node: &RawNode<FileMetadata>,
    extensions: Option<&[String]>,
) -> bool {
    match extensions {
        Some(exts) => filesystem_node_allowed(node, exts),
        None => true,
    }
}

fn filesystem_node_allowed(node: &RawNode<FileMetadata>, extensions: &[String]) -> bool {
    node.is_dir || file_name_matches_extensions(&node.name, extensions)
}

fn file_name_matches_extensions(name: &str, extensions: &[String]) -> bool {
    let normalized_name = name.to_lowercase();
    extensions
        .iter()
        .any(|extension| normalized_name.ends_with(&extension.to_lowercase()))
}

fn filesystem_search_haystack(node: &RawNode<FileMetadata>) -> String {
    format!("{} {}", node.name, node.full_path.to_string_lossy())
}

pub fn build_torrent_preview_filter(
    search_query: &str,
    search_mode: SearchMode,
) -> TreeFilter<TorrentPreviewPayload> {
    if search_query.trim().is_empty() {
        return TreeFilter::default();
    }

    match search_mode {
        SearchMode::Fuzzy => {
            let matcher = SkimMatcherV2::default();
            let query = search_query.to_lowercase();
            TreeFilter::rule_only(search_query, move |node| {
                let haystack = torrent_preview_search_haystack(node).to_lowercase();
                matcher.fuzzy_match(&haystack, &query).is_some()
            })
        }
        SearchMode::Regex => {
            let regex = regex::RegexBuilder::new(search_query)
                .case_insensitive(true)
                .build();
            match regex {
                Ok(regex) => TreeFilter::rule_only(search_query, move |node| {
                    regex.is_match(&torrent_preview_search_haystack(node))
                }),
                Err(_) => TreeFilter::rule_only(search_query, |_| false),
            }
        }
    }
}

fn torrent_preview_search_haystack(node: &RawNode<TorrentPreviewPayload>) -> String {
    format!("{} {}", node.name, node.full_path.to_string_lossy())
}

pub fn handle_filesystem_navigation(
    key_code: KeyCode,
    ctx: BrowserFilesystemNavContext<'_>,
) -> BrowserFsReduceResult {
    if let Some(action) = map_filesystem_key_to_action(key_code) {
        reduce_filesystem_navigation_action(
            action,
            BrowserFilesystemReduceContext {
                state: ctx.state,
                data: ctx.data,
                browser_mode: ctx.browser_mode,
                search_state: ctx.search_state,
                search_query: ctx.search_query,
                search_mode: ctx.search_mode,
                list_height: ctx.list_height,
            },
        )
    } else {
        BrowserFsReduceResult {
            consumed: false,
            effects: Vec::new(),
        }
    }
}

pub fn reduce_browser_dialog_action(
    action: BrowserDialogAction,
    state: &TreeViewState,
    data: &[RawNode<FileMetadata>],
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
                    resolve_confirm_decision(state, data, browser_mode),
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
                result.effects.push(BrowserDialogEffect::CleanupPendingLink);
            }

            result.effects.push(BrowserDialogEffect::ClearSearch);
            result
                .effects
                .push(BrowserDialogEffect::ToNormalAndClearPending);
        }
        BrowserDialogAction::CancelDownloadSelection => {
            if has_pending_torrent_link {
                result.effects.push(BrowserDialogEffect::CleanupPendingLink);
            }
            result.effects.push(BrowserDialogEffect::ClearSearch);
            result
                .effects
                .push(BrowserDialogEffect::ToNormalAndClearPending);
        }
    }

    result
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
            active_pane: crate::app::ConfigPane::Settings,
            editing: None,
            reset_confirmation: None,
            network_interface_selection_pending: false,
            network_interface_inventory: Default::default(),
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
            active_pane: crate::app::ConfigPane::Settings,
            editing: None,
            reset_confirmation: None,
            network_interface_selection_pending: false,
            network_interface_inventory: Default::default(),
        });
    }
    None
}

pub fn selected_torrent_file_for_confirm(
    state: &TreeViewState,
    data: &[RawNode<FileMetadata>],
    browser_mode: &FileBrowserMode,
) -> Option<std::path::PathBuf> {
    if let FileBrowserMode::File(extensions) = browser_mode {
        if let Some(path) = selected_filesystem_file_path(state, data).cloned() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if file_name_matches_extensions(name, extensions) {
                return Some(path);
            }
        }
    }
    None
}

pub fn resolve_confirm_decision(
    state: &TreeViewState,
    data: &[RawNode<FileMetadata>],
    browser_mode: &FileBrowserMode,
) -> ConfirmDecision {
    if let Some(config_ui) = confirm_config_path_selection(state, browser_mode) {
        return ConfirmDecision::ToConfig(config_ui);
    }
    if let Some(payload) = build_download_confirm_payload(state, browser_mode) {
        return ConfirmDecision::Download(payload);
    }
    if let Some(path) = selected_torrent_file_for_confirm(state, data, browser_mode) {
        return ConfirmDecision::File(path);
    }
    ConfirmDecision::None
}

pub fn build_download_confirm_payload(
    state: &TreeViewState,
    browser_mode: &FileBrowserMode,
) -> Option<DownloadConfirmPayload> {
    if let FileBrowserMode::DownloadLocSelection {
        target,
        container_name,
        use_container,
        preview_tree,
        ..
    } = browser_mode
    {
        let base_path = state.current_path.clone();
        let is_unhydrated_pending_magnet = pending_magnet_metadata_editing_locked(browser_mode);
        let container_name_to_use = if *use_container {
            if is_unhydrated_pending_magnet {
                None
            } else {
                Some(container_name.clone())
            }
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
            target: target.clone(),
            has_preview_files: !preview_tree.is_empty(),
        });
    }
    None
}

#[cfg(test)]
pub fn pending_link_info_hash(pending_torrent_link: &str) -> Option<Vec<u8>> {
    if pending_torrent_link.is_empty() {
        return None;
    }
    let (btih, btmh) = crate::app::parse_hybrid_hashes(pending_torrent_link);
    btih.or(btmh)
}

pub fn apply_priority_cycle(
    nodes: &mut [RawNode<TorrentPreviewPayload>],
    target_path: &Path,
) -> bool {
    for node in &mut *nodes {
        let found = node.find_and_act(target_path, &mut |target_node| {
            let new_priority = target_node.payload.priority.next();
            target_node.apply_recursive(&|n| {
                n.payload.priority = new_priority;
            });
        });

        if found {
            refresh_torrent_preview_directory_priorities(nodes);
            return true;
        }
    }
    false
}

pub fn expand_preview_tree(
    preview_state: &mut TreeViewState,
    preview_tree: &[RawNode<TorrentPreviewPayload>],
) {
    for node in preview_tree {
        node.expand_all(preview_state);
    }
}

pub fn collapse_preview_tree(
    preview_state: &mut TreeViewState,
    preview_tree: &[RawNode<TorrentPreviewPayload>],
) {
    preview_state.expanded_paths.clear();
    preview_state.top_most_offset = 0;

    let cursor_is_root = match &preview_state.cursor_path {
        Some(cursor_path) => preview_tree
            .iter()
            .any(|node| node.full_path == *cursor_path),
        None => false,
    };
    if !cursor_is_root {
        preview_state.cursor_path = preview_tree.first().map(|node| node.full_path.clone());
    }
}

fn common_file_priority(nodes: &[RawNode<TorrentPreviewPayload>]) -> Option<FilePriority> {
    let mut common = None;
    let mut is_mixed = false;
    collect_common_file_priority(nodes, &mut common, &mut is_mixed);

    if is_mixed {
        Some(FilePriority::Mixed)
    } else {
        common
    }
}

fn collect_common_file_priority(
    nodes: &[RawNode<TorrentPreviewPayload>],
    common: &mut Option<FilePriority>,
    is_mixed: &mut bool,
) {
    for node in nodes {
        if node.is_dir {
            collect_common_file_priority(&node.children, common, is_mixed);
            continue;
        }

        match common {
            Some(priority) if *priority != node.payload.priority => {
                *is_mixed = true;
            }
            Some(_) => {}
            None => {
                *common = Some(node.payload.priority);
            }
        }
    }
}

pub fn apply_priority_cycle_to_all(nodes: &mut [RawNode<TorrentPreviewPayload>]) -> bool {
    let Some(next_priority) = common_file_priority(nodes).map(|priority| priority.next()) else {
        return false;
    };

    for node in &mut *nodes {
        node.apply_recursive(&|target| {
            target.payload.priority = next_priority;
        });
    }
    refresh_torrent_preview_directory_priorities(nodes);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{
        AppCommand, AppRuntimeMode, BrowserPane, ConfigItem, TorrentDisplayState, TorrentMetrics,
        TorrentPreviewPayload,
    };
    use crate::config::Settings;
    use crate::terminal_event::KeyModifiers;
    use crate::tui::tree::{RawNode, TreeViewState};
    use ratatui::{backend::TestBackend, Terminal};
    use std::path::PathBuf;

    async fn app_with_existing_torrent(mode: AppMode) -> App {
        let settings = Settings {
            client_port: 0,
            ..Default::default()
        };
        let mut app = App::new(settings, AppRuntimeMode::Normal)
            .await
            .expect("build app");
        let info_hash = vec![7; 20];
        app.app_state.mode = mode;
        app.app_state.torrents.insert(
            info_hash.clone(),
            TorrentDisplayState {
                latest_state: TorrentMetrics {
                    info_hash: info_hash.clone(),
                    torrent_name: "Sample Packet".to_string(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        app.open_existing_torrent_file_browser(info_hash);
        app.app_state.ui.file_browser.fetch_pending = false;
        app
    }

    async fn handle_event(event: CrosstermEvent, app: &mut App) {
        let browser_generation = app.app_state.ui.file_browser.browser_generation;
        let reduced = super::handle_event(event, &mut app.app_state);
        crate::tui::runtime::execute_browser_fs_effects(
            app,
            browser_generation,
            reduced.fs_effects,
        )
        .await;
        crate::tui::runtime::execute_browser_dialog_effects(app, reduced.dialog_effects).await;
    }

    fn editable_download_mode(container_name: &str, backup: &str) -> FileBrowserMode {
        FileBrowserMode::DownloadLocSelection {
            target: DownloadSelectionTarget::PendingAdd,
            torrent_files: vec![],
            container_name: container_name.to_string(),
            use_container: true,
            is_editing_name: true,
            focused_pane: BrowserPane::TorrentPreview,
            preview_tree: vec![],
            preview_state: TreeViewState::default(),
            cursor_pos: container_name.len(),
            original_name_backup: backup.to_string(),
        }
    }

    fn filesystem_node(
        name: &str,
        path: impl Into<PathBuf>,
        is_dir: bool,
    ) -> RawNode<FileMetadata> {
        RawNode {
            name: name.to_string(),
            full_path: path.into(),
            children: vec![],
            payload: FileMetadata {
                size: 0,
                modified: std::time::UNIX_EPOCH,
            },
            is_dir,
        }
    }

    #[tokio::test]
    async fn escape_from_existing_torrent_browser_returns_to_torrent_management_origin() {
        let mut app = app_with_existing_torrent(AppMode::TorrentManagement).await;

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &mut app,
        )
        .await;

        assert!(matches!(app.app_state.mode, AppMode::TorrentManagement));
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn escape_from_normal_existing_torrent_browser_still_returns_to_normal() {
        let mut app = app_with_existing_torrent(AppMode::Normal).await;

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &mut app,
        )
        .await;

        assert!(matches!(app.app_state.mode, AppMode::Normal));
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn confirm_existing_torrent_browser_returns_to_torrent_management_origin() {
        let mut app = app_with_existing_torrent(AppMode::TorrentManagement).await;

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::NONE)),
            &mut app,
        )
        .await;

        assert!(matches!(app.app_state.mode, AppMode::TorrentManagement));
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn confirm_is_ignored_while_filesystem_fetch_is_pending() {
        let mut app = app_with_existing_torrent(AppMode::TorrentManagement).await;
        app.app_state.ui.file_browser.fetch_pending = true;

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::NONE)),
            &mut app,
        )
        .await;

        assert!(matches!(app.app_state.mode, AppMode::FileBrowser));
        let _ = app.shutdown_tx.send(());
    }

    #[test]
    fn search_reducer_clears_on_escape() {
        let mut search_state = BrowserSearchState::Editing;
        let mut query = String::from("abc");
        let mut search_mode = SearchMode::Fuzzy;
        let action = map_search_key_to_browser_action(KeyEvent::from(KeyCode::Esc), search_state)
            .expect("expected search action");
        let out = reduce_browser_action(action, &mut search_state, &mut query, &mut search_mode);
        assert!(out.redraw);
        assert_eq!(search_state, BrowserSearchState::Closed);
        assert!(query.is_empty());
    }

    #[test]
    fn applied_search_escape_closes_search_before_dialog() {
        let mut search_state = BrowserSearchState::Applied;
        let mut query = String::from("abc");
        let mut search_mode = SearchMode::Regex;
        let action = map_search_key_to_browser_action(KeyEvent::from(KeyCode::Esc), search_state)
            .expect("expected applied search escape action");

        let out = reduce_browser_action(action, &mut search_state, &mut query, &mut search_mode);

        assert!(out.redraw);
        assert_eq!(search_state, BrowserSearchState::Closed);
        assert!(query.is_empty());
    }

    #[test]
    fn closed_search_escape_falls_through_to_dialog() {
        assert!(map_search_key_to_browser_action(
            KeyEvent::from(KeyCode::Esc),
            BrowserSearchState::Closed
        )
        .is_none());
    }

    #[tokio::test]
    async fn name_edit_escape_wins_over_applied_search_and_restores_backup() {
        let mut app = app_with_existing_torrent(AppMode::Normal).await;
        app.app_state.ui.file_browser.browser_mode =
            editable_download_mode("changed name", "original name");
        app.app_state.ui.file_browser.search_state = BrowserSearchState::Applied;
        app.app_state.ui.file_browser.search_query = "still applied".to_string();

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &mut app,
        )
        .await;

        assert_eq!(
            app.app_state.ui.file_browser.search_state,
            BrowserSearchState::Applied
        );
        assert_eq!(app.app_state.ui.file_browser.search_query, "still applied");
        assert!(matches!(
            &app.app_state.ui.file_browser.browser_mode,
            FileBrowserMode::DownloadLocSelection {
                container_name,
                is_editing_name: false,
                ..
            } if container_name == "original name"
        ));
        let _ = app.shutdown_tx.send(());
    }

    #[tokio::test]
    async fn name_edit_treats_dialog_and_search_shortcuts_as_text() {
        let mut app = app_with_existing_torrent(AppMode::Normal).await;
        app.app_state.ui.file_browser.browser_mode = editable_download_mode("folder", "folder");

        for key_code in [KeyCode::Char('Y'), KeyCode::Char('/')] {
            handle_event(
                CrosstermEvent::Key(KeyEvent::new(key_code, KeyModifiers::NONE)),
                &mut app,
            )
            .await;
        }

        assert!(matches!(app.app_state.mode, AppMode::FileBrowser));
        assert!(matches!(
            &app.app_state.ui.file_browser.browser_mode,
            FileBrowserMode::DownloadLocSelection {
                container_name,
                is_editing_name: true,
                ..
            } if container_name == "folderY/"
        ));
        assert_eq!(
            app.app_state.ui.file_browser.search_state,
            BrowserSearchState::Closed
        );
        let _ = app.shutdown_tx.send(());
    }

    #[test]
    fn reducer_search_char_appends_and_consumes() {
        let mut search_state = BrowserSearchState::Editing;
        let mut query = String::from("ab");
        let mut search_mode = SearchMode::Fuzzy;

        let out = reduce_browser_action(
            BrowserAction::Char('c'),
            &mut search_state,
            &mut query,
            &mut search_mode,
        );

        assert!(out.redraw);
        assert_eq!(search_state, BrowserSearchState::Editing);
        assert_eq!(query, "abc");
    }

    #[test]
    fn reducer_search_noop_still_consumes_when_searching() {
        let mut search_state = BrowserSearchState::Editing;
        let mut query = String::from("abc");
        let mut search_mode = SearchMode::Fuzzy;

        let out = reduce_browser_action(
            BrowserAction::Noop,
            &mut search_state,
            &mut query,
            &mut search_mode,
        );

        assert!(out.redraw);
        assert_eq!(search_state, BrowserSearchState::Editing);
        assert_eq!(query, "abc");
    }

    #[test]
    fn reducer_search_tab_toggles_mode() {
        let mut search_state = BrowserSearchState::Editing;
        let mut query = String::from("abc");
        let mut search_mode = SearchMode::Fuzzy;
        let action = map_search_key_to_browser_action(KeyEvent::from(KeyCode::Tab), search_state)
            .expect("expected mode toggle");

        let out = reduce_browser_action(action, &mut search_state, &mut query, &mut search_mode);

        assert!(out.redraw);
        assert_eq!(search_state, BrowserSearchState::Editing);
        assert_eq!(query, "abc");
        assert_eq!(search_mode, SearchMode::Regex);
    }

    #[test]
    fn clear_browser_search_disables_and_clears_query() {
        let mut search_state = BrowserSearchState::Editing;
        let mut query = String::from("abc");

        clear_browser_search(&mut search_state, &mut query);

        assert_eq!(search_state, BrowserSearchState::Closed);
        assert!(query.is_empty());
    }

    #[test]
    fn reducer_search_enter_applies_non_empty_query() {
        let mut search_state = BrowserSearchState::Editing;
        let mut query = String::from("abc");
        let mut search_mode = SearchMode::Regex;

        let out = reduce_browser_action(
            BrowserAction::Enter,
            &mut search_state,
            &mut query,
            &mut search_mode,
        );

        assert!(out.redraw);
        assert_eq!(search_state, BrowserSearchState::Applied);
        assert_eq!(query, "abc");
    }

    #[test]
    fn reducer_filesystem_start_search_sets_flag_and_clears_query() {
        let mut search_state = BrowserSearchState::Closed;
        let mut query = String::from("abc");
        let mut state = TreeViewState {
            top_most_offset: 12,
            ..Default::default()
        };
        let data: Vec<RawNode<FileMetadata>> = vec![];
        let mode = FileBrowserMode::Directory;
        let mut search_mode = SearchMode::Fuzzy;

        let out = reduce_filesystem_navigation_action(
            BrowserFsAction::StartSearch,
            BrowserFilesystemReduceContext {
                state: &mut state,
                data: &data,
                browser_mode: &mode,
                search_state: &mut search_state,
                search_query: &mut query,
                search_mode: &mut search_mode,
                list_height: 5,
            },
        );

        assert!(out.consumed);
        assert_eq!(search_state, BrowserSearchState::Editing);
        assert!(query.is_empty());
        assert_eq!(search_mode, SearchMode::Regex);
        assert_eq!(state.top_most_offset, 0);
    }

    #[test]
    fn reducer_filesystem_enter_dir_emits_fetch_effect() {
        let mut search_state = BrowserSearchState::Editing;
        let mut query = String::new();
        let mut state = TreeViewState {
            current_path: PathBuf::from("."),
            cursor_path: Some(PathBuf::from(".")),
            ..Default::default()
        };
        let data = vec![filesystem_node("current", ".", true)];
        let mode = FileBrowserMode::Directory;
        let mut search_mode = SearchMode::Fuzzy;

        let out = reduce_filesystem_navigation_action(
            BrowserFsAction::EnterDir,
            BrowserFilesystemReduceContext {
                state: &mut state,
                data: &data,
                browser_mode: &mode,
                search_state: &mut search_state,
                search_query: &mut query,
                search_mode: &mut search_mode,
                list_height: 5,
            },
        );

        assert!(out.consumed);
        assert_eq!(search_state, BrowserSearchState::Closed);
        assert!(query.is_empty());
        assert_eq!(out.effects.len(), 1);
        assert!(matches!(
            out.effects[0],
            BrowserFsEffect::FetchFileTree { ref path, highlight_path: None, .. }
                if path == &PathBuf::from(".")
        ));
    }

    #[test]
    fn reducer_filesystem_enter_does_not_open_filtered_out_cursor() {
        let alpha_path = PathBuf::from("alpha-folder");
        let beta_path = PathBuf::from("beta-folder");
        let data = vec![
            filesystem_node("alpha-folder", alpha_path.clone(), true),
            filesystem_node("beta-folder", beta_path, true),
        ];
        let mut state = TreeViewState {
            current_path: PathBuf::from("."),
            cursor_path: Some(alpha_path),
            ..Default::default()
        };
        let mode = FileBrowserMode::Directory;
        let mut search_state = BrowserSearchState::Applied;
        let mut query = "beta".to_string();
        let mut search_mode = SearchMode::Fuzzy;

        let out = reduce_filesystem_navigation_action(
            BrowserFsAction::EnterDir,
            BrowserFilesystemReduceContext {
                state: &mut state,
                data: &data,
                browser_mode: &mode,
                search_state: &mut search_state,
                search_query: &mut query,
                search_mode: &mut search_mode,
                list_height: 5,
            },
        );

        assert!(out.consumed);
        assert!(out.effects.is_empty());
        assert_eq!(search_state, BrowserSearchState::Applied);
        assert_eq!(query, "beta");
    }

    #[test]
    fn reducer_download_edit_insert_updates_buffer_and_cursor() {
        let mut mode = FileBrowserMode::DownloadLocSelection {
            target: DownloadSelectionTarget::PendingAdd,
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
    fn reducer_download_edit_commit_accepts_safe_single_component() {
        let mut name = String::from("renamed folder");
        let mut is_editing_name = true;
        let mut cursor_pos = name.len();

        reduce_download_name_edit_action(
            BrowserDownloadEditAction::Commit,
            &mut name,
            &mut is_editing_name,
            &mut cursor_pos,
            "original folder",
        );

        assert!(!is_editing_name);
        assert_eq!(name, "renamed folder");
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
            target: DownloadSelectionTarget::PendingAdd,
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
    fn map_download_key_locks_awaiting_magnet_metadata_edits() {
        let mode = FileBrowserMode::DownloadLocSelection {
            target: DownloadSelectionTarget::PendingAdd,
            torrent_files: vec![],
            container_name: AWAITING_MAGNET_METADATA_LABEL.to_string(),
            use_container: true,
            is_editing_name: false,
            focused_pane: BrowserPane::FileSystem,
            preview_tree: vec![],
            preview_state: TreeViewState::default(),
            cursor_pos: AWAITING_MAGNET_METADATA_LABEL.len(),
            original_name_backup: AWAITING_MAGNET_METADATA_LABEL.to_string(),
        };

        assert!(map_download_key_to_action(KeyCode::Char('r'), &mode).is_none());
        assert!(map_download_key_to_action(KeyCode::Char('x'), &mode).is_none());
        assert!(matches!(
            map_download_key_to_action(KeyCode::Tab, &mode),
            Some(BrowserDownloadAction::Shortcut(
                BrowserDownloadShortcutAction::TogglePane
            ))
        ));
    }

    #[test]
    fn map_download_key_locks_active_name_edit_while_awaiting_magnet_metadata() {
        let mode = FileBrowserMode::DownloadLocSelection {
            target: DownloadSelectionTarget::PendingAdd,
            torrent_files: vec![],
            container_name: AWAITING_MAGNET_METADATA_LABEL.to_string(),
            use_container: true,
            is_editing_name: true,
            focused_pane: BrowserPane::TorrentPreview,
            preview_tree: vec![],
            preview_state: TreeViewState::default(),
            cursor_pos: AWAITING_MAGNET_METADATA_LABEL.len(),
            original_name_backup: AWAITING_MAGNET_METADATA_LABEL.to_string(),
        };

        assert!(map_download_key_to_action(KeyCode::Char('c'), &mode).is_none());
        assert!(map_download_key_to_action(KeyCode::Backspace, &mode).is_none());
    }

    #[test]
    fn map_download_key_allows_pending_magnet_edits_after_hydration() {
        let mode = FileBrowserMode::DownloadLocSelection {
            target: DownloadSelectionTarget::PendingAdd,
            torrent_files: vec![],
            container_name: "Sample Files".to_string(),
            use_container: true,
            is_editing_name: false,
            focused_pane: BrowserPane::TorrentPreview,
            preview_tree: vec![RawNode {
                name: "item.bin".to_string(),
                full_path: PathBuf::from("item.bin"),
                children: vec![],
                payload: TorrentPreviewPayload::default(),
                is_dir: false,
            }],
            preview_state: TreeViewState::default(),
            cursor_pos: 0,
            original_name_backup: "Sample Files".to_string(),
        };

        assert!(matches!(
            map_download_key_to_action(KeyCode::Char('r'), &mode),
            Some(BrowserDownloadAction::Shortcut(
                BrowserDownloadShortcutAction::StartRename
            ))
        ));
        assert!(matches!(
            map_download_key_to_action(KeyCode::Char('x'), &mode),
            Some(BrowserDownloadAction::Shortcut(
                BrowserDownloadShortcutAction::ToggleUseContainer
            ))
        ));
    }

    #[test]
    fn reduce_browser_download_action_ignores_locked_awaiting_magnet_edits() {
        let mut mode = FileBrowserMode::DownloadLocSelection {
            target: DownloadSelectionTarget::PendingAdd,
            torrent_files: vec![],
            container_name: AWAITING_MAGNET_METADATA_LABEL.to_string(),
            use_container: true,
            is_editing_name: false,
            focused_pane: BrowserPane::FileSystem,
            preview_tree: vec![],
            preview_state: TreeViewState::default(),
            cursor_pos: AWAITING_MAGNET_METADATA_LABEL.len(),
            original_name_backup: AWAITING_MAGNET_METADATA_LABEL.to_string(),
        };

        let toggle_out = reduce_browser_download_action(
            BrowserDownloadAction::Shortcut(BrowserDownloadShortcutAction::ToggleUseContainer),
            &mut mode,
        );
        assert!(!toggle_out.consumed);
        let edit_out = reduce_browser_download_action(
            BrowserDownloadAction::Edit(BrowserDownloadEditAction::Insert('x')),
            &mut mode,
        );
        assert!(!edit_out.consumed);

        let FileBrowserMode::DownloadLocSelection {
            container_name,
            use_container,
            ..
        } = mode
        else {
            panic!("expected DownloadLocSelection");
        };
        assert_eq!(container_name, AWAITING_MAGNET_METADATA_LABEL);
        assert!(use_container);
    }

    #[test]
    fn reduce_browser_download_shortcut_updates_mode() {
        let mut mode = FileBrowserMode::DownloadLocSelection {
            target: DownloadSelectionTarget::PendingAdd,
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
            target: DownloadSelectionTarget::PendingAdd,
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
    fn has_preview_content_matches_case_insensitive_torrent_extension() {
        let directory = tempfile::tempdir().expect("create tempdir");
        let path = directory.path().join("sample.TORRENT");
        std::fs::write(&path, []).expect("create selected file");
        let mode = FileBrowserMode::File(vec![".torrent".to_string()]);
        assert!(has_preview_content(&mode, false, false, Some(&path)));
    }

    #[test]
    fn preview_selection_ignores_directory_with_torrent_suffix() {
        let path = PathBuf::from("folder.torrent");
        let state = TreeViewState {
            cursor_path: Some(path.clone()),
            ..Default::default()
        };
        let data = vec![filesystem_node("folder.torrent", path, true)];
        let mode = FileBrowserMode::File(vec![".torrent".to_string()]);

        assert!(selected_filesystem_file_path(&state, &data).is_none());
        assert!(!preview_content_for_selection(
            &mode, false, false, &state, &data
        ));
    }

    #[test]
    fn preview_selection_accepts_non_directory_data_node() {
        let path = PathBuf::from("sample.TORRENT");
        let state = TreeViewState {
            cursor_path: Some(path.clone()),
            ..Default::default()
        };
        let data = vec![filesystem_node("sample.TORRENT", path.clone(), false)];
        let mode = FileBrowserMode::File(vec![".torrent".to_string()]);

        assert_eq!(selected_filesystem_file_path(&state, &data), Some(&path));
        assert!(preview_content_for_selection(
            &mode, false, false, &state, &data
        ));
    }

    #[test]
    fn browser_fetch_error_message_is_visible_and_single_line() {
        assert_eq!(
            file_browser_fetch_error_message("Directory unavailable\ntry another path"),
            "   Error: Directory unavailable?try another path"
        );
    }

    #[tokio::test]
    async fn browser_draw_renders_fetch_error_in_filesystem_panel() {
        let mut app = app_with_existing_torrent(AppMode::FileBrowser).await;
        app.app_state.ui.file_browser.browser_mode = FileBrowserMode::Directory;
        app.app_state.ui.file_browser.data.clear();
        app.app_state.ui.file_browser.fetch_error =
            Some("Directory unavailable for this location".to_string());

        let dht_status = crate::dht_service::DhtStatus::default();
        let dht_wave_telemetry = crate::dht_service::DhtWaveTelemetry::default();
        let theme = ThemeContext::new(app.app_state.theme, app.app_state.ui.effects_phase_time);
        let screen = ScreenContext::new(
            &app.app_state,
            &dht_status,
            &dht_wave_telemetry,
            &app.client_configs,
            &theme,
        );
        let browser = &app.app_state.ui.file_browser;
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("create test terminal");

        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &screen,
                    &browser.state,
                    &browser.data,
                    &browser.browser_mode,
                );
            })
            .expect("draw browser");

        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("Error: Directory unavailable for this location"));
        let _ = app.shutdown_tx.send(());
    }

    #[test]
    fn has_preview_content_shows_existing_torrent_priority_pane() {
        let mode = FileBrowserMode::DownloadLocSelection {
            target: DownloadSelectionTarget::ExistingTorrent {
                info_hash: vec![7; 20],
            },
            torrent_files: vec![],
            container_name: "Sample Files".to_string(),
            use_container: true,
            is_editing_name: false,
            focused_pane: BrowserPane::TorrentPreview,
            preview_tree: vec![],
            preview_state: TreeViewState::default(),
            cursor_pos: 0,
            original_name_backup: "Sample Files".to_string(),
        };

        assert!(has_preview_content(&mode, false, false, None));
    }

    #[test]
    fn preview_search_starts_only_from_preview_pane() {
        let mut mode = FileBrowserMode::DownloadLocSelection {
            target: DownloadSelectionTarget::PendingAdd,
            torrent_files: vec![],
            container_name: "Sample Files".to_string(),
            use_container: true,
            is_editing_name: false,
            focused_pane: BrowserPane::TorrentPreview,
            preview_tree: vec![],
            preview_state: TreeViewState::default(),
            cursor_pos: 0,
            original_name_backup: "Sample Files".to_string(),
        };

        assert!(preview_search_should_start(KeyCode::Char('/'), &mode));

        if let FileBrowserMode::DownloadLocSelection { focused_pane, .. } = &mut mode {
            *focused_pane = BrowserPane::FileSystem;
        }
        assert!(!preview_search_should_start(KeyCode::Char('/'), &mode));
    }

    #[test]
    fn torrent_preview_fuzzy_filter_matches_relative_path() {
        let tree = sample_preview_tree();
        let state = TreeViewState::default();
        let filter = build_torrent_preview_filter("alpb", SearchMode::Fuzzy);

        let rows = TreeMathHelper::get_visible_slice(&tree, &state, filter, 10);

        assert!(rows.iter().any(|row| row.node.name == "alpha.bin"));
        assert!(!rows.iter().any(|row| row.node.name == "beta.bin"));
    }

    #[test]
    fn torrent_preview_regex_filter_is_case_insensitive() {
        let tree = sample_preview_tree();
        let state = TreeViewState::default();
        let filter = build_torrent_preview_filter(r"group/[A-Z]+\.bin", SearchMode::Regex);

        let rows = TreeMathHelper::get_visible_slice(&tree, &state, filter, 10);

        assert!(rows.iter().any(|row| row.node.name == "alpha.bin"));
        assert!(rows.iter().any(|row| row.node.name == "beta.bin"));
    }

    #[test]
    fn torrent_preview_invalid_regex_matches_no_rows() {
        let tree = sample_preview_tree();
        let state = TreeViewState::default();
        let filter = build_torrent_preview_filter("[", SearchMode::Regex);

        let rows = TreeMathHelper::get_visible_slice(&tree, &state, filter, 10);

        assert!(rows.is_empty());
    }

    #[test]
    fn filesystem_fuzzy_filter_matches_relative_path() {
        let tree = sample_filesystem_tree();
        let state = TreeViewState::default();
        let filter =
            build_filesystem_filter(&FileBrowserMode::Directory, "alpb", SearchMode::Fuzzy);

        let rows = TreeMathHelper::get_visible_slice(&tree, &state, filter, 10);

        assert!(rows.iter().any(|row| row.node.name == "alpha.bin"));
        assert!(!rows.iter().any(|row| row.node.name == "beta.bin"));
    }

    #[test]
    fn filesystem_regex_filter_is_case_insensitive() {
        let tree = sample_filesystem_tree();
        let state = TreeViewState::default();
        let filter = build_filesystem_filter(
            &FileBrowserMode::Directory,
            r"group/[A-Z]+\.bin",
            SearchMode::Regex,
        );

        let rows = TreeMathHelper::get_visible_slice(&tree, &state, filter, 10);

        assert!(rows.iter().any(|row| row.node.name == "alpha.bin"));
        assert!(rows.iter().any(|row| row.node.name == "beta.bin"));
    }

    #[test]
    fn file_mode_extension_filter_is_case_insensitive() {
        let tree = vec![
            filesystem_node("sample.TORRENT", "sample.TORRENT", false),
            filesystem_node("notes.txt", "notes.txt", false),
        ];
        let state = TreeViewState::default();
        let filter = build_filesystem_filter(
            &FileBrowserMode::File(vec![".torrent".to_string()]),
            "",
            SearchMode::Regex,
        );

        let rows = TreeMathHelper::get_visible_slice(&tree, &state, filter, 10);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].node.name, "sample.TORRENT");
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
            TreeFilter::default(),
            Some(10),
        );
        assert!(out.consumed);
        assert_eq!(state.cursor_path, Some(PathBuf::from("root/child")));
    }

    #[test]
    fn preview_reducer_navigates_filtered_rows() {
        let mut tree = sample_preview_tree();
        let mut state = TreeViewState {
            cursor_path: Some(PathBuf::from("root")),
            ..Default::default()
        };

        let out = reduce_browser_preview_action(
            map_preview_key_to_action(KeyCode::Down),
            &mut state,
            &mut tree,
            build_torrent_preview_filter("beta", SearchMode::Fuzzy),
            Some(10),
        );

        assert!(out.consumed);
        assert_eq!(state.cursor_path, Some(PathBuf::from("root/group")));

        let out = reduce_browser_preview_action(
            map_preview_key_to_action(KeyCode::Down),
            &mut state,
            &mut tree,
            build_torrent_preview_filter("beta", SearchMode::Fuzzy),
            Some(10),
        );

        assert!(out.consumed);
        assert_eq!(
            state.cursor_path,
            Some(PathBuf::from("root/group/beta.bin"))
        );
    }

    #[test]
    fn preview_reducer_passes_through_confirm_key() {
        let mut tree: Vec<RawNode<TorrentPreviewPayload>> = vec![];
        let mut state = TreeViewState::default();
        let out = reduce_browser_preview_action(
            map_preview_key_to_action(KeyCode::Char('Y')),
            &mut state,
            &mut tree,
            TreeFilter::default(),
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
            TreeFilter::default(),
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
            TreeFilter::default(),
            Some(10),
        );

        assert!(out.consumed);
        assert_eq!(tree[0].payload.priority, FilePriority::Skip);
    }

    #[test]
    fn preview_reducer_cycles_priority_on_p() {
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
            map_preview_key_to_action(KeyCode::Char('p')),
            &mut state,
            &mut tree,
            TreeFilter::default(),
            Some(10),
        );

        assert!(out.consumed);
        assert_eq!(tree[0].payload.priority, FilePriority::Skip);
    }

    #[test]
    fn preview_reducer_does_not_cycle_hidden_filtered_cursor() {
        let mut tree = sample_preview_tree();
        let mut state = TreeViewState {
            cursor_path: Some(PathBuf::from("root/group/alpha.bin")),
            ..Default::default()
        };

        let out = reduce_browser_preview_action(
            map_preview_key_to_action(KeyCode::Char('p')),
            &mut state,
            &mut tree,
            build_torrent_preview_filter("beta", SearchMode::Fuzzy),
            Some(10),
        );

        assert!(out.consumed);
        assert_eq!(
            tree[0].children[0].children[0].payload.priority,
            FilePriority::Normal
        );
        assert_eq!(
            tree[0].children[0].children[1].payload.priority,
            FilePriority::Normal
        );
    }

    #[test]
    fn preview_reducer_does_not_cycle_cursor_outside_visible_window() {
        let mut tree = sample_preview_tree();
        let mut state = TreeViewState {
            cursor_path: Some(PathBuf::from("root/group/beta.bin")),
            ..Default::default()
        };
        state.expanded_paths.insert(PathBuf::from("root"));
        state.expanded_paths.insert(PathBuf::from("root/group"));

        let out = reduce_browser_preview_action(
            map_preview_key_to_action(KeyCode::Char('p')),
            &mut state,
            &mut tree,
            TreeFilter::default(),
            Some(2),
        );

        assert!(out.consumed);
        assert_eq!(
            tree[0].children[0].children[1].payload.priority,
            FilePriority::Normal
        );
    }

    #[test]
    fn download_name_edit_preserves_utf8_boundaries() {
        let mut name = String::from("Café Seed");
        let mut is_editing = true;
        let mut cursor_pos = "Café".len();

        reduce_download_name_edit_action(
            BrowserDownloadEditAction::Backspace,
            &mut name,
            &mut is_editing,
            &mut cursor_pos,
            "Café Seed",
        );

        assert_eq!(name, "Caf Seed");
        assert!(name.is_char_boundary(cursor_pos));
    }

    #[test]
    fn filesystem_cursor_visible_respects_search_filter() {
        let tree = sample_filesystem_tree();
        let mode = FileBrowserMode::File(vec![".bin".to_string()]);
        let state = TreeViewState {
            cursor_path: Some(PathBuf::from("root/group/alpha.bin")),
            ..Default::default()
        };

        assert!(filesystem_cursor_visible(
            &tree,
            &state,
            &mode,
            "alpha",
            SearchMode::Fuzzy,
            10,
        ));
        assert!(!filesystem_cursor_visible(
            &tree,
            &state,
            &mode,
            "beta",
            SearchMode::Fuzzy,
            10,
        ));
    }

    #[test]
    fn filesystem_cursor_visible_rejects_cursor_below_visible_window() {
        let tree = sample_filesystem_tree();
        let mode = FileBrowserMode::File(vec![".bin".to_string()]);
        let mut state = TreeViewState {
            cursor_path: Some(PathBuf::from("root/group/beta.bin")),
            ..Default::default()
        };
        state.expanded_paths.insert(PathBuf::from("root"));
        state.expanded_paths.insert(PathBuf::from("root/group"));

        assert!(!filesystem_cursor_visible(
            &tree,
            &state,
            &mode,
            "",
            SearchMode::Fuzzy,
            2,
        ));
    }

    #[test]
    fn search_reconciliation_moves_offscreen_cursor_into_visible_window() {
        let tree = sample_filesystem_tree();
        let mut state = TreeViewState {
            cursor_path: Some(PathBuf::from("root/group/beta.bin")),
            top_most_offset: 2,
            ..Default::default()
        };
        state.expanded_paths.insert(PathBuf::from("root"));
        state.expanded_paths.insert(PathBuf::from("root/group"));

        reconcile_tree_cursor_with_visible_window(
            &mut state,
            &tree,
            build_filesystem_filter(&FileBrowserMode::Directory, "", SearchMode::Regex),
            2,
        );

        assert_eq!(state.top_most_offset, 0);
        assert_eq!(state.cursor_path, Some(PathBuf::from("root")));
    }

    #[test]
    fn search_reconciliation_clears_cursor_when_filter_has_no_rows() {
        let tree = sample_filesystem_tree();
        let mut state = TreeViewState {
            cursor_path: Some(PathBuf::from("root/group/alpha.bin")),
            top_most_offset: 3,
            ..Default::default()
        };

        reconcile_tree_cursor_with_visible_window(
            &mut state,
            &tree,
            build_filesystem_filter(&FileBrowserMode::Directory, "no-match", SearchMode::Regex),
            5,
        );

        assert_eq!(state.top_most_offset, 0);
        assert_eq!(state.cursor_path, None);
    }

    #[test]
    fn preview_keymap_includes_bulk_preview_shortcuts() {
        assert_eq!(
            map_preview_key_to_action(KeyCode::Char('P')),
            BrowserPreviewAction::CycleAllPriorities
        );
        assert_eq!(
            map_preview_key_to_action(KeyCode::Char('e')),
            BrowserPreviewAction::ExpandAll
        );
        assert_eq!(
            map_preview_key_to_action(KeyCode::Char('c')),
            BrowserPreviewAction::CollapseAll
        );
    }

    #[test]
    fn preview_reducer_expands_and_collapses_all() {
        let mut tree = vec![RawNode {
            name: "root".to_string(),
            full_path: PathBuf::from("root"),
            children: vec![RawNode {
                name: "group".to_string(),
                full_path: PathBuf::from("root/group"),
                children: vec![RawNode {
                    name: "leaf".to_string(),
                    full_path: PathBuf::from("root/group/leaf"),
                    children: vec![],
                    payload: TorrentPreviewPayload::default(),
                    is_dir: false,
                }],
                payload: TorrentPreviewPayload::default(),
                is_dir: true,
            }],
            payload: TorrentPreviewPayload::default(),
            is_dir: true,
        }];
        let mut state = TreeViewState {
            cursor_path: Some(PathBuf::from("root/group/leaf")),
            ..Default::default()
        };

        let out = reduce_browser_preview_action(
            map_preview_key_to_action(KeyCode::Char('e')),
            &mut state,
            &mut tree,
            TreeFilter::default(),
            Some(10),
        );
        assert!(out.consumed);
        assert!(state.expanded_paths.contains(&PathBuf::from("root")));
        assert!(state.expanded_paths.contains(&PathBuf::from("root/group")));

        let out = reduce_browser_preview_action(
            map_preview_key_to_action(KeyCode::Char('c')),
            &mut state,
            &mut tree,
            TreeFilter::default(),
            Some(10),
        );
        assert!(out.consumed);
        assert!(state.expanded_paths.is_empty());
        assert_eq!(state.cursor_path, Some(PathBuf::from("root")));
    }

    #[test]
    fn preview_reducer_cycles_all_file_priorities_on_p() {
        let mut tree = vec![RawNode {
            name: "root".to_string(),
            full_path: PathBuf::from("root"),
            children: vec![
                RawNode {
                    name: "alpha.bin".to_string(),
                    full_path: PathBuf::from("root/alpha.bin"),
                    children: vec![],
                    payload: TorrentPreviewPayload::default(),
                    is_dir: false,
                },
                RawNode {
                    name: "beta.bin".to_string(),
                    full_path: PathBuf::from("root/beta.bin"),
                    children: vec![],
                    payload: TorrentPreviewPayload::default(),
                    is_dir: false,
                },
            ],
            payload: TorrentPreviewPayload::default(),
            is_dir: true,
        }];
        let mut state = TreeViewState {
            cursor_path: Some(PathBuf::from("root")),
            ..Default::default()
        };

        let out = reduce_browser_preview_action(
            map_preview_key_to_action(KeyCode::Char('P')),
            &mut state,
            &mut tree,
            TreeFilter::default(),
            Some(10),
        );

        assert!(out.consumed);
        assert_eq!(tree[0].payload.priority, FilePriority::Skip);
        assert_eq!(tree[0].children[0].payload.priority, FilePriority::Skip);
        assert_eq!(tree[0].children[1].payload.priority, FilePriority::Skip);
    }

    #[test]
    fn filesystem_navigation_starts_search() {
        let mut state = TreeViewState {
            top_most_offset: 12,
            ..Default::default()
        };
        let data: Vec<RawNode<FileMetadata>> = vec![];
        let mode = FileBrowserMode::Directory;
        let mut search_state = BrowserSearchState::Closed;
        let mut query = String::from("abc");
        let mut search_mode = SearchMode::Fuzzy;
        let reduced = handle_filesystem_navigation(
            KeyCode::Char('/'),
            BrowserFilesystemNavContext {
                state: &mut state,
                data: &data,
                browser_mode: &mode,
                search_state: &mut search_state,
                search_query: &mut query,
                search_mode: &mut search_mode,
                list_height: 5,
            },
        );
        assert!(reduced.consumed);
        assert!(reduced.effects.is_empty());
        assert_eq!(search_state, BrowserSearchState::Editing);
        assert!(query.is_empty());
        assert_eq!(search_mode, SearchMode::Regex);
        assert_eq!(state.top_most_offset, 0);
    }

    #[test]
    fn filesystem_navigation_emits_fetch_effect() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let mut state = TreeViewState {
            current_path: dir.path().to_path_buf(),
            cursor_path: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let data = vec![filesystem_node(
            "selected-folder",
            dir.path().to_path_buf(),
            true,
        )];
        let mode = FileBrowserMode::Directory;
        let mut search_state = BrowserSearchState::Closed;
        let mut query = String::new();
        let mut search_mode = SearchMode::Regex;

        let reduced = handle_filesystem_navigation(
            KeyCode::Enter,
            BrowserFilesystemNavContext {
                state: &mut state,
                data: &data,
                browser_mode: &mode,
                search_state: &mut search_state,
                search_query: &mut query,
                search_mode: &mut search_mode,
                list_height: 5,
            },
        );

        assert!(reduced.consumed);
        assert!(matches!(
            reduced.effects.as_slice(),
            [BrowserFsEffect::FetchFileTree {
                ref path,
                highlight_path: None,
                ..
            }] if path == dir.path()
        ));
    }

    fn sample_preview_tree() -> Vec<RawNode<TorrentPreviewPayload>> {
        vec![RawNode {
            name: "root".to_string(),
            full_path: PathBuf::from("root"),
            children: vec![RawNode {
                name: "group".to_string(),
                full_path: PathBuf::from("root/group"),
                children: vec![
                    RawNode {
                        name: "alpha.bin".to_string(),
                        full_path: PathBuf::from("root/group/alpha.bin"),
                        children: vec![],
                        payload: TorrentPreviewPayload::default(),
                        is_dir: false,
                    },
                    RawNode {
                        name: "beta.bin".to_string(),
                        full_path: PathBuf::from("root/group/beta.bin"),
                        children: vec![],
                        payload: TorrentPreviewPayload::default(),
                        is_dir: false,
                    },
                ],
                payload: TorrentPreviewPayload::default(),
                is_dir: true,
            }],
            payload: TorrentPreviewPayload::default(),
            is_dir: true,
        }]
    }

    fn sample_filesystem_tree() -> Vec<RawNode<FileMetadata>> {
        vec![RawNode {
            name: "root".to_string(),
            full_path: PathBuf::from("root"),
            children: vec![RawNode {
                name: "group".to_string(),
                full_path: PathBuf::from("root/group"),
                children: vec![
                    RawNode {
                        name: "alpha.bin".to_string(),
                        full_path: PathBuf::from("root/group/alpha.bin"),
                        children: vec![],
                        payload: FileMetadata {
                            size: 1,
                            modified: std::time::UNIX_EPOCH,
                        },
                        is_dir: false,
                    },
                    RawNode {
                        name: "beta.bin".to_string(),
                        full_path: PathBuf::from("root/group/beta.bin"),
                        children: vec![],
                        payload: FileMetadata {
                            size: 1,
                            modified: std::time::UNIX_EPOCH,
                        },
                        is_dir: false,
                    },
                ],
                payload: FileMetadata {
                    size: 0,
                    modified: std::time::UNIX_EPOCH,
                },
                is_dir: true,
            }],
            payload: FileMetadata {
                size: 0,
                modified: std::time::UNIX_EPOCH,
            },
            is_dir: true,
        }]
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
        let decision = resolve_confirm_decision(&state, &[], &mode);
        assert!(matches!(
            decision,
            ConfirmDecision::ToConfig(ConfigUiState { .. })
        ));
    }

    #[test]
    fn selected_file_confirm_accepts_case_insensitive_extension_for_actual_file() {
        let directory = tempfile::tempdir().expect("create tempdir");
        let path = directory.path().join("sample.TORRENT");
        std::fs::write(&path, []).expect("create selected file");
        let state = TreeViewState {
            cursor_path: Some(path.clone()),
            ..Default::default()
        };
        let mode = FileBrowserMode::File(vec![".torrent".to_string()]);

        let data = vec![filesystem_node("sample.TORRENT", path.clone(), false)];
        assert_eq!(
            selected_torrent_file_for_confirm(&state, &data, &mode),
            Some(path)
        );
    }

    #[tokio::test]
    async fn uppercase_torrent_selection_queues_add_command_before_closing_browser() {
        let directory = tempfile::tempdir().expect("create tempdir");
        let path = directory.path().join("sample.TORRENT");
        std::fs::write(&path, []).expect("create selected file");
        let mut app = app_with_existing_torrent(AppMode::Normal).await;
        while app.app_command_rx.try_recv().is_ok() {}
        app.app_state.mode = AppMode::FileBrowser;
        app.app_state.screen_area = Rect::new(0, 0, 120, 40);
        app.app_state.ui.file_browser.browser_mode =
            FileBrowserMode::File(vec![".torrent".to_string()]);
        app.app_state.ui.file_browser.state.cursor_path = Some(path.clone());
        app.app_state.ui.file_browser.data =
            vec![filesystem_node("sample.TORRENT", path.clone(), false)];
        app.app_state.ui.file_browser.fetch_pending = false;

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::NONE)),
            &mut app,
        )
        .await;

        let queued =
            tokio::time::timeout(std::time::Duration::from_secs(1), app.app_command_rx.recv())
                .await
                .expect("timed out waiting for add command")
                .expect("app command channel closed");
        assert!(matches!(
            queued,
            AppCommand::AddTorrentFromFile(queued_path) if queued_path == path
        ));
        assert!(matches!(app.app_state.mode, AppMode::Normal));
        let _ = app.shutdown_tx.send(());
    }

    #[test]
    fn selected_file_confirm_rejects_directory_with_matching_suffix() {
        let directory = tempfile::tempdir().expect("create tempdir");
        let path = directory.path().join("folder.torrent");
        std::fs::create_dir(&path).expect("create suffixed directory");
        let state = TreeViewState {
            cursor_path: Some(path.clone()),
            ..Default::default()
        };
        let mode = FileBrowserMode::File(vec![".torrent".to_string()]);

        let data = vec![filesystem_node("folder.torrent", path, true)];
        assert_eq!(
            selected_torrent_file_for_confirm(&state, &data, &mode),
            None
        );
    }

    #[test]
    fn reducer_dialog_confirm_emits_execute_and_clear_search() {
        let mode = FileBrowserMode::Directory;
        let state = TreeViewState::default();

        let out = reduce_browser_dialog_action(
            BrowserDialogAction::ConfirmSelection,
            &state,
            &[],
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

        let out =
            reduce_browser_dialog_action(BrowserDialogAction::Escape, &state, &[], &mode, true);

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

        let out =
            reduce_browser_dialog_action(BrowserDialogAction::Escape, &state, &[], &mode, true);

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
            target: DownloadSelectionTarget::PendingAdd,
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

        let out =
            reduce_browser_dialog_action(BrowserDialogAction::Escape, &state, &[], &mode, true);

        assert_eq!(out.effects.len(), 3);
        assert!(matches!(
            out.effects[0],
            BrowserDialogEffect::CleanupPendingLink
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
            target: DownloadSelectionTarget::PendingAdd,
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
            &[],
            &mode,
            true,
        );

        assert_eq!(out.effects.len(), 3);
        assert!(matches!(
            out.effects[0],
            BrowserDialogEffect::CleanupPendingLink
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
    fn awaiting_magnet_metadata_confirms_without_placeholder_container_name() {
        let mode = FileBrowserMode::DownloadLocSelection {
            target: DownloadSelectionTarget::PendingAdd,
            torrent_files: vec![],
            container_name: AWAITING_MAGNET_METADATA_LABEL.to_string(),
            use_container: true,
            is_editing_name: false,
            focused_pane: BrowserPane::FileSystem,
            preview_tree: vec![],
            preview_state: TreeViewState::default(),
            cursor_pos: 1,
            original_name_backup: AWAITING_MAGNET_METADATA_LABEL.to_string(),
        };
        let state = TreeViewState::default();

        let payload = build_download_confirm_payload(&state, &mode)
            .expect("unhydrated magnet can still be confirmed");

        assert_eq!(payload.container_name_to_use, None);
        assert!(payload.file_priorities.is_empty());
        assert!(!payload.has_preview_files);
    }

    #[test]
    fn awaiting_magnet_metadata_confirm_keeps_disabled_container_empty() {
        let mode = FileBrowserMode::DownloadLocSelection {
            target: DownloadSelectionTarget::PendingAdd,
            torrent_files: vec![],
            container_name: AWAITING_MAGNET_METADATA_LABEL.to_string(),
            use_container: false,
            is_editing_name: false,
            focused_pane: BrowserPane::FileSystem,
            preview_tree: vec![],
            preview_state: TreeViewState::default(),
            cursor_pos: 1,
            original_name_backup: AWAITING_MAGNET_METADATA_LABEL.to_string(),
        };
        let state = TreeViewState::default();

        let payload = build_download_confirm_payload(&state, &mode)
            .expect("unhydrated magnet can be confirmed without a container");

        assert_eq!(payload.container_name_to_use, Some(String::new()));
    }

    #[test]
    fn pending_link_info_hash_falls_back_to_btmh_only_magnet() {
        let pending_link = concat!(
            "magnet:?xt=urn:btmh:1220",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );

        let info_hash = pending_link_info_hash(pending_link).expect("btmh-only info hash");

        assert_eq!(info_hash.len(), 20);
        assert!(info_hash.iter().all(|byte| *byte == 0xaa));
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
