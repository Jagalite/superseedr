// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Stateful browser-owned presentation and input adapter.

use ratatui::{layout::Rect, Terminal};
use superseedr::terminal_event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers,
};
use superseedr::web_integration::{BrowserSession, BrowserTorrentControlState};
use wasm_bindgen::prelude::*;

use crate::ansi_backend::AnsiBackend;
use crate::mocks::DemoCommandService;

const KEY_KIND_PRESS: u8 = 0;
const KEY_KIND_REPEAT: u8 = 1;
const KEY_KIND_RELEASE: u8 = 2;
const FIXTURE_HASH_HEX: &str = "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a";

/// Retained terminal session used by the permanent browser shell.
#[wasm_bindgen]
pub struct BrowserDemo {
    terminal: Terminal<AnsiBackend>,
    session: BrowserSession,
    service: DemoCommandService,
}

#[wasm_bindgen]
impl BrowserDemo {
    #[wasm_bindgen(constructor)]
    pub fn new(columns: u16, rows: u16) -> Self {
        let columns = columns.max(1);
        let rows = rows.max(1);
        let backend = AnsiBackend::new(columns, rows);
        let mut terminal =
            Terminal::new(backend).expect("ANSI backend initialization is infallible");
        terminal
            .clear()
            .expect("ANSI backend clearing is infallible");

        Self {
            terminal,
            session: BrowserSession::from_fixture(columns, rows, crate::milestone_one_fixture()),
            service: DemoCommandService::default(),
        }
    }

    #[wasm_bindgen(js_name = renderFrame)]
    pub fn render_frame(&mut self) -> String {
        self.terminal
            .draw(|frame| self.session.draw(frame))
            .expect("ANSI rendering is infallible");
        self.terminal.backend_mut().take_output()
    }

    /// Invalidates the retained Ratatui buffer and returns a self-contained frame.
    #[wasm_bindgen(js_name = forceRefresh)]
    pub fn force_refresh(&mut self) -> String {
        self.terminal
            .clear()
            .expect("ANSI backend clearing is infallible");
        self.render_frame()
    }

    #[wasm_bindgen(js_name = dispatchKey)]
    pub async fn dispatch_key(
        &mut self,
        key: String,
        modifier_bits: u8,
        kind: u8,
    ) -> bool {
        let Some(code) = key_code(&key) else {
            return false;
        };
        let Some(kind) = key_kind(kind) else {
            return false;
        };
        let modifiers = KeyModifiers::from_bits_truncate(modifier_bits);
        self.session
            .dispatch_event(Event::Key(KeyEvent::new_with_kind_and_state(
                code,
                modifiers,
                kind,
                KeyEventState::NONE,
            )))
            .await;
        let _ = self.service.fulfill_pending(&mut self.session);
        true
    }

    #[wasm_bindgen(js_name = dispatchPaste)]
    pub async fn dispatch_paste(&mut self, text: String) {
        self.session.dispatch_event(Event::Paste(text)).await;
        let _ = self.service.fulfill_pending(&mut self.session);
    }

    #[wasm_bindgen(js_name = flushInput)]
    pub async fn flush_input(&mut self) {
        self.session.flush_pending_paste_burst().await;
        let _ = self.service.fulfill_pending(&mut self.session);
    }

    #[wasm_bindgen]
    pub async fn resize(&mut self, columns: u16, rows: u16) {
        let columns = columns.max(1);
        let rows = rows.max(1);
        self.terminal.backend_mut().resize(columns, rows);
        self.terminal
            .resize(Rect::new(0, 0, columns, rows))
            .expect("ANSI backend resizing is infallible");
        self.session
            .dispatch_event(Event::Resize(columns, rows))
            .await;
        let _ = self.service.fulfill_pending(&mut self.session);
    }

    #[wasm_bindgen(getter, js_name = columns)]
    pub fn columns(&self) -> u16 {
        self.session.screen_size().0
    }

    #[wasm_bindgen(getter, js_name = rows)]
    pub fn rows(&self) -> u16 {
        self.session.screen_size().1
    }

    #[wasm_bindgen(getter, js_name = selectedTorrentPaused)]
    pub fn selected_torrent_paused(&self) -> bool {
        matches!(
            self.session.torrent_control_state_hex(FIXTURE_HASH_HEX),
            Some(BrowserTorrentControlState::Paused)
        )
    }

    #[wasm_bindgen(getter, js_name = torrentCount)]
    pub fn torrent_count(&self) -> usize {
        self.session.torrent_count()
    }
}

fn key_kind(kind: u8) -> Option<KeyEventKind> {
    match kind {
        KEY_KIND_PRESS => Some(KeyEventKind::Press),
        KEY_KIND_REPEAT => Some(KeyEventKind::Repeat),
        KEY_KIND_RELEASE => Some(KeyEventKind::Release),
        _ => None,
    }
}

fn key_code(key: &str) -> Option<KeyCode> {
    Some(match key {
        "Backspace" => KeyCode::Backspace,
        "Enter" => KeyCode::Enter,
        "ArrowLeft" => KeyCode::Left,
        "ArrowRight" => KeyCode::Right,
        "ArrowUp" => KeyCode::Up,
        "ArrowDown" => KeyCode::Down,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "PageUp" => KeyCode::PageUp,
        "PageDown" => KeyCode::PageDown,
        "Tab" => KeyCode::Tab,
        "Delete" => KeyCode::Delete,
        "Insert" => KeyCode::Insert,
        "Escape" => KeyCode::Esc,
        " " => KeyCode::Char(' '),
        value => {
            let mut characters = value.chars();
            let character = characters.next()?;
            if characters.next().is_some() {
                return None;
            }
            KeyCode::Char(character)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    #[wasm_bindgen_test]
    fn key_adapter_accepts_named_and_character_keys() {
        assert_eq!(key_code("Escape"), Some(KeyCode::Esc));
        assert_eq!(key_code("ArrowDown"), Some(KeyCode::Down));
        assert_eq!(key_code("D"), Some(KeyCode::Char('D')));
        assert_eq!(key_code("Unidentified"), None);
    }

    #[wasm_bindgen_test]
    fn key_kind_adapter_rejects_unknown_values() {
        assert_eq!(key_kind(KEY_KIND_PRESS), Some(KeyEventKind::Press));
        assert_eq!(key_kind(KEY_KIND_REPEAT), Some(KeyEventKind::Repeat));
        assert_eq!(key_kind(KEY_KIND_RELEASE), Some(KeyEventKind::Release));
        assert_eq!(key_kind(3), None);
    }
}
