// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use super::terminal_event::{KeyCode, KeyEvent};
use std::time::Duration;
use web_time::Instant;

#[derive(Default)]
pub struct PasteBurst<const CHARACTER_INTERVAL_MS: u64> {
    queued_keys: Vec<KeyEvent>,
    queued_text: String,
    last_plain_char_at: Option<Instant>,
}

pub enum FlushResult {
    None,
    Buffered,
    Text(String),
    Keys(Vec<KeyEvent>),
}

impl<const CHARACTER_INTERVAL_MS: u64> PasteBurst<CHARACTER_INTERVAL_MS> {
    const CHARACTER_INTERVAL: Duration = Duration::from_millis(CHARACTER_INTERVAL_MS);

    pub fn next_deadline(&self) -> Option<Instant> {
        self.last_plain_char_at
            .map(|instant| instant + Self::CHARACTER_INTERVAL)
    }

    pub fn has_pending(&self) -> bool {
        !self.queued_keys.is_empty()
    }

    pub fn pending_text(&self) -> &str {
        &self.queued_text
    }

    pub fn is_due(&self, now: Instant) -> bool {
        self.last_plain_char_at
            .is_some_and(|last| now.duration_since(last) > Self::CHARACTER_INTERVAL)
    }

    pub fn push_key(&mut self, key: KeyEvent, now: Instant) -> FlushResult {
        let stale_result = if self
            .last_plain_char_at
            .is_some_and(|last| now.duration_since(last) > Self::CHARACTER_INTERVAL)
        {
            self.drain_as_keys()
        } else {
            FlushResult::None
        };

        if let KeyCode::Char(ch) = key.code {
            self.queued_keys.push(key);
            self.queued_text.push(ch);
            self.last_plain_char_at = Some(now);
        }

        if matches!(stale_result, FlushResult::None) {
            FlushResult::Buffered
        } else {
            stale_result
        }
    }

    pub fn flush_if_due<F>(&mut self, now: Instant, should_treat_as_paste: F) -> FlushResult
    where
        F: FnOnce(&str) -> bool,
    {
        if !self.is_due(now) {
            return FlushResult::None;
        }
        self.finish_flush(should_treat_as_paste)
    }

    pub fn flush_now<F>(&mut self, should_treat_as_paste: F) -> FlushResult
    where
        F: FnOnce(&str) -> bool,
    {
        self.finish_flush(should_treat_as_paste)
    }

    pub fn clear(&mut self) {
        self.queued_keys.clear();
        self.queued_text.clear();
        self.last_plain_char_at = None;
    }

    fn finish_flush<F>(&mut self, should_treat_as_paste: F) -> FlushResult
    where
        F: FnOnce(&str) -> bool,
    {
        if self.queued_keys.is_empty() {
            self.clear();
            return FlushResult::None;
        }

        if should_treat_as_paste(&self.queued_text) {
            let text = std::mem::take(&mut self.queued_text);
            self.queued_keys.clear();
            self.last_plain_char_at = None;
            return FlushResult::Text(text);
        }

        self.drain_as_keys()
    }

    fn drain_as_keys(&mut self) -> FlushResult {
        if self.queued_keys.is_empty() {
            self.clear();
            return FlushResult::None;
        }

        let keys = std::mem::take(&mut self.queued_keys);
        self.queued_text.clear();
        self.last_plain_char_at = None;
        FlushResult::Keys(keys)
    }

    #[cfg(test)]
    pub fn flush_delay() -> Duration {
        Self::CHARACTER_INTERVAL + Duration::from_millis(1)
    }
}

#[cfg(test)]
mod tests {
    use super::super::terminal_event::KeyModifiers;
    use super::*;

    type TestPasteBurst = PasteBurst<8>;

    #[test]
    fn single_key_flushes_as_keys_when_not_paste() {
        let mut burst = TestPasteBurst::default();
        let start = Instant::now();
        let result = burst.push_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), start);
        assert!(matches!(result, FlushResult::Buffered));

        let result = burst.flush_if_due(start + TestPasteBurst::flush_delay(), |_| false);
        assert!(matches!(result, FlushResult::Keys(keys) if keys.len() == 1));
    }

    #[test]
    fn magnet_like_burst_flushes_as_text() {
        let mut burst = TestPasteBurst::default();
        let start = Instant::now();
        for (offset, ch) in ['m', 'a', 'g', 'n', 'e', 't', ':'].into_iter().enumerate() {
            let _ = burst.push_key(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                start + Duration::from_millis(offset as u64),
            );
        }

        let result = burst.flush_if_due(
            start + Duration::from_millis(6) + TestPasteBurst::flush_delay(),
            |text| text.starts_with("magnet:"),
        );
        assert!(matches!(result, FlushResult::Text(text) if text == "magnet:"));
    }

    #[test]
    fn interruption_flushes_pending_keys_without_leaking_state() {
        let mut burst = TestPasteBurst::default();
        let start = Instant::now();
        let _ = burst.push_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), start);
        let _ = burst.push_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            start + Duration::from_millis(1),
        );

        let result = burst.flush_now(|_| false);
        assert!(matches!(result, FlushResult::Keys(keys) if keys.len() == 2));
        assert!(!burst.has_pending());
    }
}
