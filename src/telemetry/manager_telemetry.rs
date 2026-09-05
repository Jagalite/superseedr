// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::TorrentMetrics;
use std::time::Duration;

#[derive(Debug, Default)]
pub struct ManagerTelemetry {
    last_sent_metrics: Option<TorrentMetrics>,
}

impl ManagerTelemetry {
    pub fn should_emit(&mut self, metrics: &TorrentMetrics) -> bool {
        let force_emit =
            metrics.bytes_downloaded_this_tick > 0 || metrics.bytes_uploaded_this_tick > 0;

        if !force_emit {
            let current_norm = Self::normalized_for_compare(metrics);
            if let Some(previous) = &self.last_sent_metrics {
                let previous_norm = Self::normalized_for_compare(previous);
                if current_norm == previous_norm {
                    return false;
                }
            }
        }

        self.last_sent_metrics = Some(metrics.clone());
        true
    }

    fn normalized_for_compare(metrics: &TorrentMetrics) -> TorrentMetrics {
        let mut normalized = metrics.clone();
        normalized.next_announce_in = Duration::ZERO;
        normalized.eta = Duration::ZERO;
        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::ManagerTelemetry;
    use crate::app::TorrentMetrics;
    use std::time::Duration;

    fn sample_metrics() -> TorrentMetrics {
        TorrentMetrics {
            info_hash: vec![1; 20],
            torrent_name: "example".to_string(),
            number_of_pieces_total: 100,
            number_of_pieces_completed: 20,
            download_speed_bps: 1024,
            upload_speed_bps: 0,
            bytes_downloaded_this_tick: 0,
            bytes_uploaded_this_tick: 0,
            eta: Duration::from_secs(120),
            activity_message: "Downloading".to_string(),
            next_announce_in: Duration::from_secs(10),
            total_size: 1_000_000,
            bytes_written: 200_000,
            ..Default::default()
        }
    }

    #[test]
    fn emits_first_snapshot() {
        let mut telemetry = ManagerTelemetry::default();
        let metrics = sample_metrics();
        assert!(telemetry.should_emit(&metrics));
    }

    #[test]
    fn suppresses_identical_snapshot() {
        let mut telemetry = ManagerTelemetry::default();
        let metrics = sample_metrics();

        assert!(telemetry.should_emit(&metrics));
        assert!(!telemetry.should_emit(&metrics));
    }

    #[test]
    fn ignores_countdown_only_drift() {
        let mut telemetry = ManagerTelemetry::default();
        let first = sample_metrics();
        let mut second = first.clone();
        second.next_announce_in = Duration::from_secs(5);
        second.eta = Duration::from_secs(110);

        assert!(telemetry.should_emit(&first));
        assert!(!telemetry.should_emit(&second));
    }

    #[test]
    fn forces_emit_when_bytes_nonzero() {
        let mut telemetry = ManagerTelemetry::default();
        let first = sample_metrics();
        let mut second = first.clone();
        second.bytes_downloaded_this_tick = 4096;

        assert!(telemetry.should_emit(&first));
        assert!(telemetry.should_emit(&second));
    }

    #[test]
    fn emits_on_meaningful_change() {
        let mut telemetry = ManagerTelemetry::default();
        let first = sample_metrics();
        let mut second = first.clone();
        second.number_of_pieces_completed += 1;

        assert!(telemetry.should_emit(&first));
        assert!(telemetry.should_emit(&second));
    }
}
