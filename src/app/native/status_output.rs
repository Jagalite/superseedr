// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native status execution. Runtime resources remain owned by `App`.

use super::*;

impl App {
    pub fn generate_output_state(&self) -> AppOutputState {
        let s = &self.app_state;
        let torrent_metrics = s
            .torrents
            .iter()
            .map(|(k, v)| (k.clone(), v.latest_state.clone()))
            .collect();

        AppOutputState {
            run_time: s.run_time,
            cpu_usage: s.cpu_usage,
            ram_usage_percent: s.ram_usage_percent,
            total_download_bps: s.avg_download_history.last().copied().unwrap_or(0),
            total_upload_bps: s.avg_upload_history.last().copied().unwrap_or(0),
            status_config: status::status_config_from_settings(&self.client_configs),
            dht: self.dht_service.current_status(),
            network: Some(
                self.network_state_rx
                    .borrow()
                    .runtime_status(&self.client_configs.network_binding),
            ),
            torrents: torrent_metrics,
        }
    }

    pub fn dump_status_to_file(&self) {
        if self.is_current_shared_follower() {
            return;
        }

        let generation = self
            .status_dump_generation
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);

        status::dump(
            self.generate_output_state(),
            self.shutdown_tx.clone(),
            self.is_current_shared_leader(),
            generation,
            self.status_dump_generation.clone(),
        );
    }

    pub(super) fn effective_status_dump_interval_secs(&self) -> u64 {
        let configured_interval = self
            .status_dump_interval_override_secs
            .unwrap_or(self.client_configs.output_status_interval);
        if configured_interval == 0 && self.is_shared_mode_enabled() {
            5
        } else {
            configured_interval
        }
    }

    pub(super) fn reschedule_status_dump_deadline(&mut self) {
        let interval_secs = self.effective_status_dump_interval_secs();
        self.next_status_dump_at = if interval_secs > 0 {
            Some(time::Instant::now() + Duration::from_secs(interval_secs))
        } else {
            None
        };
    }

    pub(super) fn trigger_status_dump_now(&mut self) {
        self.dump_status_to_file();
        self.reschedule_status_dump_deadline();
    }

    pub(super) fn trigger_status_dump_after_successful_cluster_mutation(&mut self) {
        if self.is_current_shared_leader() {
            self.trigger_status_dump_now();
        }
    }

    pub(super) fn set_runtime_status_dump_interval_override(&mut self, interval_secs: Option<u64>) {
        self.status_dump_interval_override_secs = interval_secs;
        self.reschedule_status_dump_deadline();
    }
}
