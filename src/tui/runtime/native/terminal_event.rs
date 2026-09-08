// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native terminal-input adapter.

use crate::tui::input::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MediaKeyCode,
    ModifierKeyCode, MouseButton, MouseEvent, MouseEventKind,
};

pub(crate) fn adapt_event(event: crossterm::event::Event) -> Event {
    use crossterm::event::Event as NativeEvent;

    match event {
        NativeEvent::FocusGained => Event::FocusGained,
        NativeEvent::FocusLost => Event::FocusLost,
        NativeEvent::Key(event) => Event::Key(adapt_key_event(event)),
        NativeEvent::Mouse(event) => Event::Mouse(MouseEvent {
            kind: adapt_mouse_kind(event.kind),
            column: event.column,
            row: event.row,
            modifiers: adapt_modifiers(event.modifiers),
        }),
        NativeEvent::Paste(text) => Event::Paste(text),
        NativeEvent::Resize(columns, rows) => Event::Resize(columns, rows),
    }
}

fn adapt_key_event(event: crossterm::event::KeyEvent) -> KeyEvent {
    KeyEvent::new_with_kind_and_state(
        adapt_key_code(event.code),
        adapt_modifiers(event.modifiers),
        match event.kind {
            crossterm::event::KeyEventKind::Press => KeyEventKind::Press,
            crossterm::event::KeyEventKind::Repeat => KeyEventKind::Repeat,
            crossterm::event::KeyEventKind::Release => KeyEventKind::Release,
        },
        KeyEventState::from_bits_retain(event.state.bits()),
    )
}

fn adapt_modifiers(modifiers: crossterm::event::KeyModifiers) -> KeyModifiers {
    KeyModifiers::from_bits_retain(modifiers.bits())
}

fn adapt_mouse_kind(kind: crossterm::event::MouseEventKind) -> MouseEventKind {
    use crossterm::event::MouseEventKind as NativeKind;

    match kind {
        NativeKind::Down(button) => MouseEventKind::Down(adapt_mouse_button(button)),
        NativeKind::Up(button) => MouseEventKind::Up(adapt_mouse_button(button)),
        NativeKind::Drag(button) => MouseEventKind::Drag(adapt_mouse_button(button)),
        NativeKind::Moved => MouseEventKind::Moved,
        NativeKind::ScrollDown => MouseEventKind::ScrollDown,
        NativeKind::ScrollUp => MouseEventKind::ScrollUp,
        NativeKind::ScrollLeft => MouseEventKind::ScrollLeft,
        NativeKind::ScrollRight => MouseEventKind::ScrollRight,
    }
}

fn adapt_mouse_button(button: crossterm::event::MouseButton) -> MouseButton {
    match button {
        crossterm::event::MouseButton::Left => MouseButton::Left,
        crossterm::event::MouseButton::Right => MouseButton::Right,
        crossterm::event::MouseButton::Middle => MouseButton::Middle,
    }
}

fn adapt_key_code(code: crossterm::event::KeyCode) -> KeyCode {
    use crossterm::event::KeyCode as NativeCode;

    match code {
        NativeCode::Backspace => KeyCode::Backspace,
        NativeCode::Enter => KeyCode::Enter,
        NativeCode::Left => KeyCode::Left,
        NativeCode::Right => KeyCode::Right,
        NativeCode::Up => KeyCode::Up,
        NativeCode::Down => KeyCode::Down,
        NativeCode::Home => KeyCode::Home,
        NativeCode::End => KeyCode::End,
        NativeCode::PageUp => KeyCode::PageUp,
        NativeCode::PageDown => KeyCode::PageDown,
        NativeCode::Tab => KeyCode::Tab,
        NativeCode::BackTab => KeyCode::BackTab,
        NativeCode::Delete => KeyCode::Delete,
        NativeCode::Insert => KeyCode::Insert,
        NativeCode::F(number) => KeyCode::F(number),
        NativeCode::Char(character) => KeyCode::Char(character),
        NativeCode::Null => KeyCode::Null,
        NativeCode::Esc => KeyCode::Esc,
        NativeCode::CapsLock => KeyCode::CapsLock,
        NativeCode::ScrollLock => KeyCode::ScrollLock,
        NativeCode::NumLock => KeyCode::NumLock,
        NativeCode::PrintScreen => KeyCode::PrintScreen,
        NativeCode::Pause => KeyCode::Pause,
        NativeCode::Menu => KeyCode::Menu,
        NativeCode::KeypadBegin => KeyCode::KeypadBegin,
        NativeCode::Media(code) => KeyCode::Media(adapt_media_key(code)),
        NativeCode::Modifier(code) => KeyCode::Modifier(adapt_modifier_key(code)),
    }
}

fn adapt_media_key(code: crossterm::event::MediaKeyCode) -> MediaKeyCode {
    use crossterm::event::MediaKeyCode as NativeCode;

    match code {
        NativeCode::Play => MediaKeyCode::Play,
        NativeCode::Pause => MediaKeyCode::Pause,
        NativeCode::PlayPause => MediaKeyCode::PlayPause,
        NativeCode::Reverse => MediaKeyCode::Reverse,
        NativeCode::Stop => MediaKeyCode::Stop,
        NativeCode::FastForward => MediaKeyCode::FastForward,
        NativeCode::Rewind => MediaKeyCode::Rewind,
        NativeCode::TrackNext => MediaKeyCode::TrackNext,
        NativeCode::TrackPrevious => MediaKeyCode::TrackPrevious,
        NativeCode::Record => MediaKeyCode::Record,
        NativeCode::LowerVolume => MediaKeyCode::LowerVolume,
        NativeCode::RaiseVolume => MediaKeyCode::RaiseVolume,
        NativeCode::MuteVolume => MediaKeyCode::MuteVolume,
    }
}

fn adapt_modifier_key(code: crossterm::event::ModifierKeyCode) -> ModifierKeyCode {
    use crossterm::event::ModifierKeyCode as NativeCode;

    match code {
        NativeCode::LeftShift => ModifierKeyCode::LeftShift,
        NativeCode::LeftControl => ModifierKeyCode::LeftControl,
        NativeCode::LeftAlt => ModifierKeyCode::LeftAlt,
        NativeCode::LeftSuper => ModifierKeyCode::LeftSuper,
        NativeCode::LeftHyper => ModifierKeyCode::LeftHyper,
        NativeCode::LeftMeta => ModifierKeyCode::LeftMeta,
        NativeCode::RightShift => ModifierKeyCode::RightShift,
        NativeCode::RightControl => ModifierKeyCode::RightControl,
        NativeCode::RightAlt => ModifierKeyCode::RightAlt,
        NativeCode::RightSuper => ModifierKeyCode::RightSuper,
        NativeCode::RightHyper => ModifierKeyCode::RightHyper,
        NativeCode::RightMeta => ModifierKeyCode::RightMeta,
        NativeCode::IsoLevel3Shift => ModifierKeyCode::IsoLevel3Shift,
        NativeCode::IsoLevel5Shift => ModifierKeyCode::IsoLevel5Shift,
    }
}

#[cfg(test)]
mod tests {
    use super::adapt_event;
    use crate::tui::input::{
        Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton,
        MouseEvent, MouseEventKind,
    };

    #[test]
    fn native_key_event_preserves_code_modifiers_kind_and_state() {
        let event = crossterm::event::Event::Key(crossterm::event::KeyEvent::new_with_kind(
            crossterm::event::KeyCode::Char('k'),
            crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::ALT,
            crossterm::event::KeyEventKind::Repeat,
        ));

        assert_eq!(
            adapt_event(event),
            Event::Key(KeyEvent::new_with_kind_and_state(
                KeyCode::Char('k'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
                KeyEventKind::Repeat,
                KeyEventState::NONE,
            ))
        );
    }

    #[test]
    fn native_mouse_event_preserves_coordinates_button_and_modifiers() {
        let event = crossterm::event::Event::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Right),
            column: 41,
            row: 17,
            modifiers: crossterm::event::KeyModifiers::SHIFT,
        });

        assert_eq!(
            adapt_event(event),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Right),
                column: 41,
                row: 17,
                modifiers: KeyModifiers::SHIFT,
            })
        );
    }

    #[test]
    fn native_non_key_events_preserve_payloads() {
        assert_eq!(
            adapt_event(crossterm::event::Event::Paste("magnet payload".into())),
            Event::Paste("magnet payload".into())
        );
        assert_eq!(
            adapt_event(crossterm::event::Event::Resize(132, 43)),
            Event::Resize(132, 43)
        );
        assert_eq!(
            adapt_event(crossterm::event::Event::FocusGained),
            Event::FocusGained
        );
        assert_eq!(
            adapt_event(crossterm::event::Event::FocusLost),
            Event::FocusLost
        );
    }
}
