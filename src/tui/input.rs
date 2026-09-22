// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Platform-neutral terminal input data used by native and browser TUI event handling.
//!
//! Native and browser shells translate their concrete input sources into this data-only model.

mod model {
    use bitflags::bitflags;
    use std::hash::{Hash, Hasher};

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Hash)]
    pub enum KeyCode {
        Backspace,
        Enter,
        Left,
        Right,
        Up,
        Down,
        Home,
        End,
        PageUp,
        PageDown,
        Tab,
        BackTab,
        Delete,
        Insert,
        F(u8),
        Char(char),
        Null,
        Esc,
        CapsLock,
        ScrollLock,
        NumLock,
        PrintScreen,
        Pause,
        Menu,
        KeypadBegin,
        Media(MediaKeyCode),
        Modifier(ModifierKeyCode),
    }

    impl KeyCode {
        pub fn is_function_key(&self, number: u8) -> bool {
            matches!(self, Self::F(candidate) if *candidate == number)
        }

        pub fn is_char(&self, character: char) -> bool {
            matches!(self, Self::Char(candidate) if *candidate == character)
        }

        pub fn as_char(&self) -> Option<char> {
            match self {
                Self::Char(character) => Some(*character),
                _ => None,
            }
        }

        pub fn is_media_key(&self, media: MediaKeyCode) -> bool {
            matches!(self, Self::Media(candidate) if *candidate == media)
        }

        pub fn is_modifier(&self, modifier: ModifierKeyCode) -> bool {
            matches!(self, Self::Modifier(candidate) if *candidate == modifier)
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Hash)]
    pub enum MediaKeyCode {
        Play,
        Pause,
        PlayPause,
        Reverse,
        Stop,
        FastForward,
        Rewind,
        TrackNext,
        TrackPrevious,
        Record,
        LowerVolume,
        RaiseVolume,
        MuteVolume,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Hash)]
    pub enum ModifierKeyCode {
        LeftShift,
        LeftControl,
        LeftAlt,
        LeftSuper,
        LeftHyper,
        LeftMeta,
        RightShift,
        RightControl,
        RightAlt,
        RightSuper,
        RightHyper,
        RightMeta,
        IsoLevel3Shift,
        IsoLevel5Shift,
    }

    bitflags! {
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Hash)]
        pub struct KeyModifiers: u8 {
            const NONE = 0;
            const SHIFT = 0b0000_0001;
            const CONTROL = 0b0000_0010;
            const ALT = 0b0000_0100;
            const SUPER = 0b0000_1000;
            const HYPER = 0b0001_0000;
            const META = 0b0010_0000;
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Hash)]
    pub enum KeyEventKind {
        Press,
        Repeat,
        Release,
    }

    bitflags! {
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Hash)]
        pub struct KeyEventState: u8 {
            const NONE = 0;
            const KEYPAD = 0b0000_0001;
            const CAPS_LOCK = 0b0000_0010;
            const NUM_LOCK = 0b0000_0100;
        }
    }

    #[derive(Clone, Copy, Debug, PartialOrd)]
    pub struct KeyEvent {
        pub code: KeyCode,
        pub modifiers: KeyModifiers,
        pub kind: KeyEventKind,
        pub state: KeyEventState,
    }

    impl KeyEvent {
        pub const fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
            Self::new_with_kind_and_state(code, modifiers, KeyEventKind::Press, KeyEventState::NONE)
        }

        pub const fn new_with_kind(
            code: KeyCode,
            modifiers: KeyModifiers,
            kind: KeyEventKind,
        ) -> Self {
            Self::new_with_kind_and_state(code, modifiers, kind, KeyEventState::NONE)
        }

        pub const fn new_with_kind_and_state(
            code: KeyCode,
            modifiers: KeyModifiers,
            kind: KeyEventKind,
            state: KeyEventState,
        ) -> Self {
            Self {
                code,
                modifiers,
                kind,
                state,
            }
        }

        pub fn is_press(&self) -> bool {
            matches!(self.kind, KeyEventKind::Press)
        }

        pub fn is_release(&self) -> bool {
            matches!(self.kind, KeyEventKind::Release)
        }

        pub fn is_repeat(&self) -> bool {
            matches!(self.kind, KeyEventKind::Repeat)
        }

        fn normalize_case(mut self) -> Self {
            let KeyCode::Char(character) = self.code else {
                return self;
            };

            if character.is_ascii_uppercase() {
                self.modifiers.insert(KeyModifiers::SHIFT);
            } else if self.modifiers.contains(KeyModifiers::SHIFT) {
                self.code = KeyCode::Char(character.to_ascii_uppercase());
            }
            self
        }
    }

    impl From<KeyCode> for KeyEvent {
        fn from(code: KeyCode) -> Self {
            Self::new(code, KeyModifiers::NONE)
        }
    }

    impl PartialEq for KeyEvent {
        fn eq(&self, other: &Self) -> bool {
            let left = self.normalize_case();
            let right = other.normalize_case();
            left.code == right.code
                && left.modifiers == right.modifiers
                && left.kind == right.kind
                && left.state == right.state
        }
    }

    impl Eq for KeyEvent {}

    impl Hash for KeyEvent {
        fn hash<H: Hasher>(&self, state: &mut H) {
            let normalized = self.normalize_case();
            normalized.code.hash(state);
            normalized.modifiers.hash(state);
            normalized.kind.hash(state);
            normalized.state.hash(state);
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Hash)]
    pub enum MouseButton {
        Left,
        Right,
        Middle,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Hash)]
    pub enum MouseEventKind {
        Down(MouseButton),
        Up(MouseButton),
        Drag(MouseButton),
        Moved,
        ScrollDown,
        ScrollUp,
        ScrollLeft,
        ScrollRight,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Hash)]
    pub struct MouseEvent {
        pub kind: MouseEventKind,
        pub column: u16,
        pub row: u16,
        pub modifiers: KeyModifiers,
    }

    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Hash)]
    pub enum Event {
        FocusGained,
        FocusLost,
        Key(KeyEvent),
        Mouse(MouseEvent),
        Paste(String),
        Resize(u16, u16),
    }

    impl Event {
        pub fn is_key_press(&self) -> bool {
            matches!(self, Self::Key(event) if event.is_press())
        }

        pub fn is_key_release(&self) -> bool {
            matches!(self, Self::Key(event) if event.is_release())
        }

        pub fn is_key_repeat(&self) -> bool {
            matches!(self, Self::Key(event) if event.is_repeat())
        }

        pub fn as_key_event(&self) -> Option<KeyEvent> {
            match self {
                Self::Key(event) => Some(*event),
                _ => None,
            }
        }

        pub fn as_key_press_event(&self) -> Option<KeyEvent> {
            self.as_key_event().filter(KeyEvent::is_press)
        }

        pub fn as_key_release_event(&self) -> Option<KeyEvent> {
            self.as_key_event().filter(KeyEvent::is_release)
        }

        pub fn as_key_repeat_event(&self) -> Option<KeyEvent> {
            self.as_key_event().filter(KeyEvent::is_repeat)
        }

        pub fn as_mouse_event(&self) -> Option<MouseEvent> {
            match self {
                Self::Mouse(event) => Some(*event),
                _ => None,
            }
        }

        pub fn as_paste_event(&self) -> Option<&str> {
            match self {
                Self::Paste(text) => Some(text),
                _ => None,
            }
        }

        pub fn as_resize_event(&self) -> Option<(u16, u16)> {
            match self {
                Self::Resize(columns, rows) => Some((*columns, *rows)),
                _ => None,
            }
        }
    }
}

pub use model::*;
