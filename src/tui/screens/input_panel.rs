// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::theme::ThemeContext;
use ratatui::layout::Rect;
use ratatui::prelude::{Frame, Line, Span, Style};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};

pub(crate) fn draw_prompt_panel(
    f: &mut Frame,
    area: Rect,
    title: String,
    value: String,
    trailing_spans: Vec<Span<'static>>,
    ctx: &ThemeContext,
) {
    draw_prompt_panel_with_cursor(f, area, title, value, trailing_spans, true, ctx);
}

pub(crate) fn draw_prompt_panel_with_cursor(
    f: &mut Frame,
    area: Rect,
    title: String,
    value: String,
    mut trailing_spans: Vec<Span<'static>>,
    show_cursor: bool,
    ctx: &ThemeContext,
) {
    let mut line_spans = vec![
        Span::styled(
            "> ",
            ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
        ),
        Span::raw(value),
    ];
    if show_cursor {
        line_spans.push(Span::styled(
            "_",
            ctx.apply(Style::default().fg(ctx.state_warning())),
        ));
    }
    line_spans.append(&mut trailing_spans);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .padding(Padding::horizontal(1))
        .border_style(ctx.apply(Style::default().fg(ctx.state_selected())));
    f.render_widget(Paragraph::new(Line::from(line_spans)).block(block), area);
}
