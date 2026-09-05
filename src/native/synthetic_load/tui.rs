// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Production rendering and telemetry, driven by the synthetic harness's main-thread loop.
//! This deliberately does not start App, load user settings, or execute application effects.

use super::{DynError, ManagerRuntime};
use crate::app::{
    advance_ui_effects_for_frame, finalize_manager_metrics_batch, reduce_app_action, AppAction,
    AppMode, AppState, DataRate,
};
use crate::config::Settings;
use crate::dht_model::{DhtStatus, DhtWaveTelemetry};
use crate::telemetry::ui_telemetry::UiTelemetry;
use crate::theme::Theme;
use crate::torrent_manager::ManagerEvent;
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    execute,
    terminal::{Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use serde::Serialize;
use std::fs::File;
use std::io::{self, BufWriter, IsTerminal, Stderr, Write};
use std::path::Path;
use std::time::{Duration, Instant};

pub(super) fn validate(enabled: bool) -> Result<(), DynError> {
    if enabled && !io::stderr().is_terminal() {
        return Err(
            "--tui requires stderr to be a terminal; redirect stdout for JSON results".into(),
        );
    }
    Ok(())
}

#[derive(Default, Serialize)]
pub(super) struct DurationSummary {
    total_us: u64,
    max_us: u64,
}

impl DurationSummary {
    fn observe(&mut self, us: u64) {
        self.total_us = self.total_us.saturating_add(us);
        self.max_us = self.max_us.max(us);
    }
}

#[derive(Default, Serialize)]
pub(super) struct TuiSummary {
    target_fps: u16,
    measured_seconds: f64,
    measured_frames: u64,
    average_fps: f64,
    frames_over_budget: u64,
    lateness: DurationSummary,
    preparation: DurationSummary,
    draw: DurationSummary,
    frame_interval: DurationSummary,
    terminal_width: u16,
    terminal_height: u16,
}

#[derive(Serialize)]
struct FrameSample {
    elapsed_us: u64,
    phase: &'static str,
    lateness_us: u64,
    preparation_us: u64,
    draw_us: u64,
    frame_interval_us: Option<u64>,
    over_budget: bool,
    manager_events_since_frame: u64,
    manager_event_work_us: u64,
    terminal_width: u16,
    terminal_height: u16,
}

impl TuiSummary {
    fn observe(&mut self, sample: &FrameSample) {
        self.terminal_width = sample.terminal_width;
        self.terminal_height = sample.terminal_height;
        if sample.phase != "measure" {
            return;
        }
        self.measured_frames += 1;
        self.frames_over_budget += u64::from(sample.over_budget);
        self.lateness.observe(sample.lateness_us);
        self.preparation.observe(sample.preparation_us);
        self.draw.observe(sample.draw_us);
        if let Some(interval) = sample.frame_interval_us {
            self.frame_interval.observe(interval);
        }
    }
}

// No raw mode: this is a passive display and Ctrl+C remains a normal OS signal.
// Construct before entering the alternate screen so partial setup errors also restore it.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stderr(), Show, LeaveAlternateScreen);
    }
}

struct BenchmarkView {
    state: AppState,
    settings: Settings,
    dht_status: DhtStatus,
    dht_wave: DhtWaveTelemetry,
    next_stats: Instant,
}

impl BenchmarkView {
    fn new(start: Instant) -> Self {
        let settings = Settings::default();
        let state = AppState {
            mode: AppMode::Normal,
            theme: Theme::builtin(settings.ui_theme),
            data_rate: DataRate::Rate60s,
            effective_download_limit_bps: settings.global_download_limit_bps,
            system_warning: Some("Synthetic benchmark · 60 FPS · Ctrl+C to stop".to_string()),
            ..AppState::default()
        };
        Self {
            state,
            settings,
            dht_status: DhtStatus::default(),
            dht_wave: DhtWaveTelemetry::default(),
            next_stats: start + Duration::from_secs(1),
        }
    }

    fn prepare(&mut self, managers: &mut [ManagerRuntime], now: Instant) {
        let mut changed = false;
        for manager in managers {
            if manager.metrics_rx.has_changed().unwrap_or(false) {
                let metrics = manager.metrics_rx.borrow_and_update().clone();
                // Persistence and lifecycle effects belong to the harness, not this view.
                let _ = reduce_app_action(
                    &mut self.state,
                    AppAction::ManagerMetrics(Box::new(metrics)),
                );
                changed = true;
            }
        }
        if changed {
            finalize_manager_metrics_batch(&mut self.state, false);
        }
        if now >= self.next_stats {
            UiTelemetry::on_second_tick_with_system_snapshot(&mut self.state, None);
            self.next_stats = now + Duration::from_secs(1);
        }
        advance_ui_effects_for_frame(
            &mut self.state,
            &self.settings,
            &self.dht_status,
            &self.dht_wave,
        );
    }
}

pub(super) struct TuiRenderer {
    terminal: Terminal<CrosstermBackend<Stderr>>,
    _guard: TerminalGuard,
    view: BenchmarkView,
    frames: BufWriter<File>,
    summary: TuiSummary,
    next_frame: Instant,
    previous_frame: Option<Instant>,
    event_count: u64,
    event_work: Duration,
}

impl TuiRenderer {
    pub(super) fn new(output_dir: &Path, start: Instant) -> Result<Self, DynError> {
        let frames = BufWriter::new(File::create(output_dir.join("tui-frames.jsonl"))?);
        let guard = TerminalGuard;
        // Terminal::clear queries the cursor through stdout in Crossterm. Keep
        // all terminal traffic on stderr, including setup, when stdout is JSON.
        execute!(
            io::stderr(),
            EnterAlternateScreen,
            Hide,
            Clear(ClearType::All),
            MoveTo(0, 0)
        )?;
        let terminal = Terminal::new(CrosstermBackend::new(io::stderr()))?;
        Ok(Self {
            terminal,
            _guard: guard,
            view: BenchmarkView::new(start),
            frames,
            summary: TuiSummary {
                target_fps: 60,
                ..TuiSummary::default()
            },
            next_frame: start,
            previous_frame: None,
            event_count: 0,
            event_work: Duration::ZERO,
        })
    }

    pub(super) fn deadline(&self) -> tokio::time::Instant {
        self.next_frame.into()
    }

    pub(super) fn handle_event(&mut self, event: ManagerEvent) {
        let start = Instant::now();
        let _ = reduce_app_action(&mut self.view.state, AppAction::ManagerEvent(event));
        self.event_work += start.elapsed();
        self.event_count += 1;
    }

    pub(super) fn frame(
        &mut self,
        managers: &mut [ManagerRuntime],
        start: Instant,
        warmup: Duration,
    ) -> Result<(), DynError> {
        let now = Instant::now();
        let period = DataRate::Rate60s.frame_interval();
        let scheduled = self.next_frame;
        self.next_frame += period;
        while self.next_frame <= now {
            self.next_frame += period;
        }
        self.view.state.ui.record_frame_wake(scheduled, now, period);
        self.view.state.ui.record_drawn_frame(now);
        self.view.prepare(managers, now);
        let draw_start = Instant::now();
        let view = &mut self.view;
        self.terminal.draw(|frame| {
            view.state.screen_area = frame.area();
            crate::tui::render::draw(
                frame,
                &view.state,
                &view.dht_status,
                &view.dht_wave,
                &view.settings,
            );
        })?;
        let finished = Instant::now();
        let draw_duration = finished.duration_since(draw_start);
        self.view
            .state
            .ui
            .record_draw_duration(draw_duration, period);
        let area = self.view.state.screen_area;
        let sample = FrameSample {
            elapsed_us: micros(now.duration_since(start)),
            phase: if now.duration_since(start) < warmup {
                "warmup"
            } else {
                "measure"
            },
            lateness_us: micros(now.saturating_duration_since(scheduled)),
            preparation_us: micros(draw_start.duration_since(now)),
            draw_us: micros(draw_duration),
            frame_interval_us: self
                .previous_frame
                .map(|previous| micros(now.duration_since(previous))),
            over_budget: finished.saturating_duration_since(scheduled) > period,
            manager_events_since_frame: self.event_count,
            manager_event_work_us: micros(self.event_work),
            terminal_width: area.width,
            terminal_height: area.height,
        };
        self.summary.observe(&sample);
        // Buffered output is outside the measured draw, but its cost can delay the next frame.
        writeln!(self.frames, "{}", serde_json::to_string(&sample)?)?;
        self.previous_frame = Some(now);
        self.event_count = 0;
        self.event_work = Duration::ZERO;
        Ok(())
    }

    pub(super) fn finish(mut self, measured: Duration) -> Result<TuiSummary, DynError> {
        self.frames.flush()?;
        self.summary.measured_seconds = measured.as_secs_f64();
        if !measured.is_zero() {
            self.summary.average_fps = self.summary.measured_frames as f64 / measured.as_secs_f64();
        }
        Ok(self.summary)
    }
}

fn micros(duration: Duration) -> u64 {
    duration.as_micros().min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::TorrentMetrics;
    use ratatui::backend::TestBackend;
    use tokio::sync::{mpsc, watch};

    #[tokio::test]
    async fn live_metrics_reach_the_production_normal_renderer() {
        let start = Instant::now();
        let mut view = BenchmarkView::new(start);
        let (metrics_tx, metrics_rx) = watch::channel(TorrentMetrics::default());
        let (command_tx, _) = mpsc::channel(1);
        let mut managers = vec![ManagerRuntime {
            metrics_rx,
            command_tx,
            handle: tokio::spawn(async { Ok(()) }),
        }];
        let hash = vec![0x51; 20];
        metrics_tx
            .send(TorrentMetrics {
                info_hash: hash.clone(),
                torrent_name: "Synthetic Meadow".to_string(),
                number_of_pieces_total: 100,
                number_of_pieces_completed: 25,
                download_speed_bps: 200_000_000,
                ..TorrentMetrics::default()
            })
            .unwrap();
        view.prepare(&mut managers, start);
        assert_eq!(view.state.torrent_list_order, vec![hash.clone()]);
        assert_eq!(
            view.state.torrents[&hash].latest_state.download_speed_bps,
            200_000_000
        );

        let mut terminal = Terminal::new(TestBackend::new(160, 50)).unwrap();
        terminal
            .draw(|frame| {
                crate::tui::render::draw(
                    frame,
                    &view.state,
                    &view.dht_status,
                    &view.dht_wave,
                    &view.settings,
                )
            })
            .unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("Synthetic Meadow"));
        managers.pop().unwrap().handle.await.unwrap().unwrap();
    }

    #[test]
    fn frame_summary_excludes_warmup_and_preserves_spikes() {
        let mut summary = TuiSummary::default();
        let mut sample = FrameSample {
            elapsed_us: 0,
            phase: "warmup",
            lateness_us: 90_000,
            preparation_us: 2_000,
            draw_us: 5_000,
            frame_interval_us: Some(100_000),
            over_budget: true,
            manager_events_since_frame: 0,
            manager_event_work_us: 0,
            terminal_width: 120,
            terminal_height: 40,
        };
        summary.observe(&sample);
        assert_eq!(summary.measured_frames, 0);
        sample.phase = "measure";
        sample.lateness_us = 20_000;
        summary.observe(&sample);
        sample.lateness_us = 1_000;
        sample.over_budget = false;
        summary.observe(&sample);
        assert_eq!(summary.measured_frames, 2);
        assert_eq!(summary.frames_over_budget, 1);
        assert_eq!(summary.lateness.total_us, 21_000);
        assert_eq!(summary.lateness.max_us, 20_000);
        assert_eq!(summary.preparation.total_us, 4_000);
        assert_eq!(summary.draw.total_us, 10_000);
    }
}
