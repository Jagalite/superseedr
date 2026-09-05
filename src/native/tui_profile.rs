// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Opt-in measurements of the production App loop, independent of the synthetic renderer.

use crate::app::{AppMode, AppState};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Default, Serialize)]
struct HandlerSample {
    count: u64,
    total_us: u64,
    max_us: u64,
}

pub(crate) struct FrameProfiler {
    writer: BufWriter<File>,
    previous_frame: Option<Instant>,
    handlers: BTreeMap<&'static str, HandlerSample>,
}

impl FrameProfiler {
    pub(crate) fn from_env() -> io::Result<Option<Self>> {
        let Some(path) = std::env::var_os("SUPERSEEDR_TUI_PROFILE") else {
            return Ok(None);
        };
        Ok(Some(Self {
            writer: BufWriter::new(File::create(path)?),
            previous_frame: None,
            handlers: BTreeMap::new(),
        }))
    }

    pub(crate) fn handler(&mut self, name: &'static str, elapsed: Duration) {
        let sample = self.handlers.entry(name).or_default();
        let us = elapsed.as_micros().min(u64::MAX as u128) as u64;
        sample.count += 1;
        sample.total_us += us;
        sample.max_us = sample.max_us.max(us);
    }

    pub(crate) fn frame(
        &mut self,
        state: &AppState,
        scheduled: Instant,
        started: Instant,
        draw_started: Instant,
        finished: Instant,
        period: Duration,
    ) -> io::Result<()> {
        let download_bytes: u64 = state
            .torrents
            .values()
            .map(|t| t.latest_state.session_total_downloaded)
            .sum();
        let written_bytes: u64 = state
            .torrents
            .values()
            .map(|t| t.latest_state.bytes_written)
            .sum();
        let peers: usize = state
            .torrents
            .values()
            .map(|t| t.latest_state.number_of_successfully_connected_peers)
            .sum();
        let record = serde_json::json!({
            "unix_ms": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis(),
            "normal_screen": matches!(state.mode, AppMode::Normal),
            "target_frame_ms": period.as_secs_f64() * 1000.0,
            "wake_lag_ms": started.saturating_duration_since(scheduled).as_secs_f64() * 1000.0,
            "preparation_ms": draw_started.duration_since(started).as_secs_f64() * 1000.0,
            "draw_ms": finished.duration_since(draw_started).as_secs_f64() * 1000.0,
            "frame_interval_ms": self.previous_frame.map(|previous| started.duration_since(previous).as_secs_f64() * 1000.0),
            "over_budget": finished.saturating_duration_since(scheduled) > period,
            "download_bytes": download_bytes,
            "written_bytes": written_bytes,
            "connected_peers": peers,
            "torrents": state.torrents.len(),
            "active_peer_limit": state.active_peer_limit,
            "base_peer_limit": state.limits.max_connected_peers,
            "app_cpu_percent": state.cpu_usage,
            "handlers_since_frame": self.handlers,
        });
        writeln!(self.writer, "{record}")?;
        self.previous_frame = Some(started);
        self.handlers.clear();
        Ok(())
    }
}
