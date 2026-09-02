// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::convert::Infallible;
use std::fmt::Write as _;

use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::{Cell, CellWidth};
use ratatui::layout::{Position, Size};
use ratatui::style::{Color, Modifier};

pub(crate) struct AnsiBackend {
    size: Size,
    cursor: Position,
    output: String,
}

impl AnsiBackend {
    pub(crate) fn new(width: u16, height: u16) -> Self {
        Self {
            size: Size::new(width.max(1), height.max(1)),
            cursor: Position::ORIGIN,
            output: String::new(),
        }
    }

    pub(crate) fn take_output(&mut self) -> String {
        if self.output.is_empty() {
            return String::new();
        }
        self.output.push_str("\x1b[0m");
        std::mem::take(&mut self.output)
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn resize(&mut self, width: u16, height: u16) {
        self.size = Size::new(width.max(1), height.max(1));
        self.cursor = Position::ORIGIN;
    }

    fn write_cell_style(&mut self, cell: &Cell) {
        self.output.push_str("\x1b[0m");
        for (modifier, code) in [
            (Modifier::BOLD, 1),
            (Modifier::DIM, 2),
            (Modifier::ITALIC, 3),
            (Modifier::UNDERLINED, 4),
            (Modifier::SLOW_BLINK, 5),
            (Modifier::RAPID_BLINK, 6),
            (Modifier::REVERSED, 7),
            (Modifier::HIDDEN, 8),
            (Modifier::CROSSED_OUT, 9),
        ] {
            if cell.modifier.contains(modifier) {
                let _ = write!(self.output, "\x1b[{code}m");
            }
        }
        write_color(&mut self.output, cell.fg, true);
        write_color(&mut self.output, cell.bg, false);
    }
}

impl Backend for AnsiBackend {
    type Error = Infallible;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut terminal_cursor: Option<Position> = None;
        let mut last_style = None;

        for (x, y, cell) in content {
            let adjacent = terminal_cursor == Some(Position::new(x, y));
            if !adjacent {
                let _ = write!(self.output, "\x1b[{};{}H", y + 1, x + 1);
            }

            let style = (cell.fg, cell.bg, cell.modifier);
            if last_style != Some(style) {
                self.write_cell_style(cell);
                last_style = Some(style);
            }

            self.output.push_str(cell.symbol());
            self.cursor = Position::new(x.saturating_add(cell.cell_width()), y);
            terminal_cursor = Some(self.cursor);
        }
        Ok(())
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.output.push_str("\x1b[?25l");
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.output.push_str("\x1b[?25h");
        Ok(())
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        Ok(self.cursor)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.cursor = position.into();
        let _ = write!(
            self.output,
            "\x1b[{};{}H",
            self.cursor.y + 1,
            self.cursor.x + 1
        );
        Ok(())
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.output.push_str("\x1b[2J");
        Ok(())
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.output.push_str(match clear_type {
            ClearType::All => "\x1b[2J",
            ClearType::AfterCursor => "\x1b[0J",
            ClearType::BeforeCursor => "\x1b[1J",
            ClearType::CurrentLine => "\x1b[2K",
            ClearType::UntilNewLine => "\x1b[0K",
        });
        Ok(())
    }

    fn size(&self) -> Result<Size, Self::Error> {
        Ok(self.size)
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        Ok(WindowSize {
            columns_rows: self.size,
            pixels: Size::new(0, 0),
        })
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn write_color(output: &mut String, color: Color, foreground: bool) {
    let base = if foreground { 30 } else { 40 };
    let bright = if foreground { 90 } else { 100 };
    let reset = if foreground { 39 } else { 49 };

    match color {
        Color::Reset => {
            let _ = write!(output, "\x1b[{reset}m");
        }
        Color::Black => write_simple_color(output, base),
        Color::Red => write_simple_color(output, base + 1),
        Color::Green => write_simple_color(output, base + 2),
        Color::Yellow => write_simple_color(output, base + 3),
        Color::Blue => write_simple_color(output, base + 4),
        Color::Magenta => write_simple_color(output, base + 5),
        Color::Cyan => write_simple_color(output, base + 6),
        Color::Gray => write_simple_color(output, base + 7),
        Color::DarkGray => write_simple_color(output, bright),
        Color::LightRed => write_simple_color(output, bright + 1),
        Color::LightGreen => write_simple_color(output, bright + 2),
        Color::LightYellow => write_simple_color(output, bright + 3),
        Color::LightBlue => write_simple_color(output, bright + 4),
        Color::LightMagenta => write_simple_color(output, bright + 5),
        Color::LightCyan => write_simple_color(output, bright + 6),
        Color::White => write_simple_color(output, bright + 7),
        Color::Rgb(red, green, blue) => {
            let channel = if foreground { 38 } else { 48 };
            let _ = write!(output, "\x1b[{channel};2;{red};{green};{blue}m");
        }
        Color::Indexed(index) => {
            let channel = if foreground { 38 } else { 48 };
            let _ = write!(output, "\x1b[{channel};5;{index}m");
        }
    }
}

fn write_simple_color(output: &mut String, code: u8) {
    let _ = write!(output, "\x1b[{code}m");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draw_encodes_position_color_and_symbol() {
        let mut backend = AnsiBackend::new(80, 24);
        let mut cell = Cell::default();
        cell.set_symbol("X").set_fg(Color::LightCyan);

        backend
            .draw(std::iter::once((2, 3, &cell)))
            .expect("infallible draw");
        let output = backend.take_output();

        assert!(output.contains("\x1b[4;3H"));
        assert!(output.contains("\x1b[96m"));
        assert!(output.contains('X'));
    }

    #[test]
    fn draw_repositions_after_a_double_width_symbol() {
        let mut backend = AnsiBackend::new(80, 24);
        let mut wide = Cell::default();
        wide.set_symbol("界");
        let continuation = Cell::default();
        let mut following = Cell::default();
        following.set_symbol("X");

        backend
            .draw(
                [
                    (0, 0, &wide),
                    (1, 0, &continuation),
                    (2, 0, &following),
                ]
                .into_iter(),
            )
            .expect("infallible draw");
        let output = backend.take_output();

        assert!(output.contains("界\x1b[1;2H"));
        assert!(output.ends_with(" X\x1b[0m"));
    }
}
