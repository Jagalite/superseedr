// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Opt-in, per-thread aggregate timings. Nested spans are inclusive; do not sum them.
//! Files are written at most once per second per active thread, plus thread exit.

use serde::Serialize;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

fn directory() -> Option<&'static PathBuf> {
    static DIRECTORY: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIRECTORY
        .get_or_init(|| {
            let path = PathBuf::from(std::env::var_os("SUPERSEEDR_PERF_PROFILE_DIR")?);
            if !path.is_absolute() {
                eprintln!("SUPERSEEDR_PERF_PROFILE_DIR must be absolute");
                return None;
            }
            if let Err(error) = fs::create_dir_all(&path) {
                eprintln!("Cannot create performance profile directory: {error}");
                return None;
            }
            Some(path)
        })
        .as_ref()
}

pub(crate) fn enabled() -> bool {
    directory().is_some()
}

#[derive(Default, Serialize)]
struct Sample {
    count: u64,
    total: u64,
    max: u64,
}

impl Sample {
    fn record(&mut self, value: u64) {
        self.count = self.count.saturating_add(1);
        self.total = self.total.saturating_add(value);
        self.max = self.max.max(value);
    }
}

struct ThreadProfile {
    writer: BufWriter<File>,
    samples: BTreeMap<&'static str, Sample>,
    window_started: Instant,
    window_unix_ms: u128,
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

impl ThreadProfile {
    fn from_env() -> Option<Self> {
        let path = directory()?.join(format!(
            "{}-{:?}.jsonl",
            std::process::id(),
            std::thread::current().id()
        ));
        let file = match File::create(path) {
            Ok(file) => file,
            Err(error) => {
                eprintln!("Cannot open performance profile: {error}");
                return None;
            }
        };
        Some(Self {
            writer: BufWriter::new(file),
            samples: BTreeMap::new(),
            window_started: Instant::now(),
            window_unix_ms: unix_ms(),
        })
    }

    fn flush(&mut self) {
        if self.samples.is_empty() {
            return;
        }
        let now = unix_ms();
        let record = serde_json::json!({
            "start_unix_ms": self.window_unix_ms,
            "end_unix_ms": now,
            "elapsed_ms": self.window_started.elapsed().as_secs_f64() * 1000.0,
            "thread": std::thread::current().name(),
            "samples": self.samples,
        });
        if let Err(error) = writeln!(self.writer, "{record}").and_then(|()| self.writer.flush()) {
            eprintln!("Cannot write performance profile: {error}");
        }
        self.samples.clear();
        self.window_started = Instant::now();
        self.window_unix_ms = now;
    }
}

impl Drop for ThreadProfile {
    fn drop(&mut self) {
        self.flush();
    }
}

thread_local! {
    static PROFILE: RefCell<Option<ThreadProfile>> = RefCell::new(ThreadProfile::from_env());
}

/// Timings have a `_ns` suffix; other values are counts or queue depths.
pub(crate) fn record(name: &'static str, value: u64) {
    if !enabled() {
        return;
    }
    PROFILE.with_borrow_mut(|profile| {
        if let Some(profile) = profile {
            profile.samples.entry(name).or_default().record(value);
            if profile.window_started.elapsed().as_secs() >= 1 {
                profile.flush();
            }
        }
    });
}

pub(crate) struct Span {
    name: &'static str,
    started: Option<Instant>,
}

impl Span {
    pub(crate) fn new(name: &'static str) -> Self {
        Self {
            name,
            started: enabled().then(Instant::now),
        }
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        if let Some(started) = self.started {
            record(
                self.name,
                started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Sample;

    #[test]
    fn aggregate_preserves_count_total_and_worst_case() {
        let mut sample = Sample::default();
        for value in [0, 9, 3, 2] {
            sample.record(value);
        }
        assert_eq!((sample.count, sample.total, sample.max), (4, 14, 9));
    }
}
