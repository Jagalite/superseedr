// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native resources execution. Runtime resources remain owned by `App`.

use super::*;

impl App {
    pub(super) fn wake_lag_peer_throttle_floor(&self, base_peer_limit: usize) -> usize {
        if base_peer_limit == 0 {
            return 0;
        }

        let minimum_floor = WAKE_LAG_PEER_THROTTLE_MIN_PEERS.min(base_peer_limit);
        if self.peer_limiter_download_activity_active() {
            let download_floor = base_peer_limit
                .saturating_mul(WAKE_LAG_PEER_THROTTLE_DOWNLOAD_FLOOR_PERCENT)
                .saturating_div(100)
                .clamp(1, base_peer_limit);
            minimum_floor.max(download_floor)
        } else {
            minimum_floor
        }
    }

    pub(super) fn peer_limiter_download_activity_active(&self) -> bool {
        self.app_state
            .avg_download_history
            .last()
            .copied()
            .unwrap_or(0)
            > 0
            || self.app_state.torrents.values().any(|torrent| {
                torrent.latest_state.torrent_control_state == TorrentControlState::Running
                    && !torrent.latest_state.is_complete
            })
    }

    pub(super) fn effective_resource_limits(&self) -> CalculatedLimits {
        let mut limits = self.app_state.limits.clone();
        let floor_peer_limit = self.wake_lag_peer_throttle_floor(limits.max_connected_peers);
        limits.max_connected_peers = self
            .wake_lag_peer_throttle
            .effective_peer_limit(limits.max_connected_peers, floor_peer_limit);
        limits
    }

    pub(super) fn peer_admission_stress_active_for(
        &self,
        effective_limits: &CalculatedLimits,
    ) -> bool {
        effective_limits.max_connected_peers < self.app_state.limits.max_connected_peers
    }

    pub(super) fn peer_admission_stress_active(&self) -> bool {
        self.peer_admission_stress_active_for(&self.effective_resource_limits())
    }

    pub(super) fn effective_peer_queue_size(&self, effective_limits: &CalculatedLimits) -> usize {
        if self.peer_admission_stress_active_for(effective_limits) {
            0
        } else {
            self.app_state.limits.max_connected_peers.saturating_mul(2)
        }
    }

    pub(super) async fn apply_effective_resource_limits(&mut self) {
        let effective_limits = self.effective_resource_limits();
        let peer_queue_size = self.effective_peer_queue_size(&effective_limits);
        self.app_state.active_peer_limit = (effective_limits.max_connected_peers
            < self.app_state.limits.max_connected_peers)
            .then_some(effective_limits.max_connected_peers);
        if self.last_applied_resource_limits.as_ref() == Some(&effective_limits)
            && self.last_applied_peer_queue_size == Some(peer_queue_size)
        {
            return;
        }

        self.last_applied_resource_limits = Some(effective_limits.clone());
        self.last_applied_peer_queue_size = Some(peer_queue_size);
        self.last_dht_peer_slot_usage = None;
        self.sync_dht_peer_slot_usage();
        let _ = self
            .resource_manager
            .update_limits_and_queue_sizes(
                effective_limits.into_map_with_peer_queue(peer_queue_size),
            )
            .await;
    }

    pub(super) fn update_wake_lag_peer_throttle(&mut self) {
        let wake_lag_frame_ratio = self.app_state.ui.frame_wake_lag_ratio_ema;
        let wake_lag_secs = self.app_state.ui.frame_wake_lag_secs_ema;
        let base_peer_limit = self.app_state.limits.max_connected_peers;
        let floor_peer_limit = self.wake_lag_peer_throttle_floor(base_peer_limit);
        let connected_peers = self.total_successfully_connected_peers();
        let change = self.wake_lag_peer_throttle.update(
            wake_lag_frame_ratio,
            wake_lag_secs,
            base_peer_limit,
            floor_peer_limit,
            connected_peers,
        );
        let effective_peer_limit = self
            .wake_lag_peer_throttle
            .effective_peer_limit(base_peer_limit, floor_peer_limit);

        if let Some(change) = change {
            tracing_event!(
                target: "superseedr::wake_lag_peer_throttle",
                Level::INFO,
                wake_lag_frame_ratio = ?wake_lag_frame_ratio,
                wake_lag_secs = ?wake_lag_secs,
                action = change.action,
                previous_peer_limit = change.previous_peer_limit,
                current_peer_limit = change.current_peer_limit,
                base_peer_limit,
                floor_peer_limit,
                effective_peer_limit,
                connected_peers,
                good_ticks = self.wake_lag_peer_throttle.good_ticks,
                "wake_lag_peer_throttle"
            );
        }
    }

    pub(super) async fn calculate_stats(&mut self, sys: &mut System) {
        let was_seeding = self.app_state.is_seeding;
        let previous_torrent_sort = self.app_state.torrent_sort;
        let previous_peer_sort = self.app_state.peer_sort;
        let previous_system_snapshot = SystemTelemetrySnapshot {
            cpu_usage: self.app_state.cpu_usage,
            ram_usage_percent: self.app_state.ram_usage_percent,
            app_ram_usage: self.app_state.app_ram_usage,
            run_time: self.app_state.run_time,
        };
        let system_snapshot = UiTelemetry::second_tick_requires_system_snapshot(&self.app_state)
            .then(|| sample_system_telemetry(sys, previous_system_snapshot))
            .flatten();
        UiTelemetry::on_second_tick_with_system_snapshot(&mut self.app_state, system_snapshot);
        self.update_disk_backpressure_download_throttle();
        align_unpinned_peer_sort_with_visible_activity(&mut self.app_state);
        if refresh_autosort_after_stats(
            &mut self.app_state,
            previous_torrent_sort,
            previous_peer_sort,
        ) {
            self.app_state.ui.needs_redraw = true;
        }
        NetworkHistoryTelemetry::on_second_tick(&mut self.app_state);
        self.tuning_controller.on_second_tick();
        self.app_state.tuning_countdown = self.tuning_controller.countdown_secs();
        self.update_wake_lag_peer_throttle();
        if was_seeding != self.app_state.is_seeding {
            self.reset_tuning_for_objective_change();
        }
        self.apply_effective_resource_limits().await;

        let history = if !self.app_state.is_seeding {
            &self.app_state.avg_download_history
        } else {
            &self.app_state.avg_upload_history
        };
        let lookback = self.tuning_controller.lookback_secs();
        let relevant_history = &history[history.len().saturating_sub(lookback)..];
        self.tuning_controller.update_live_score(
            relevant_history,
            self.app_state.global_disk_thrash_score,
            self.app_state.adaptive_max_scpb,
        );
        self.sync_tuning_state_from_controller();
        ActivityHistoryTelemetry::on_second_tick(&mut self.app_state);
    }

    pub(super) fn update_disk_backpressure_download_throttle(&mut self) {
        let sample = DiskBackpressureSample {
            is_leeching: !self.app_state.is_seeding,
            configured_download_limit_bps: self.client_configs.global_download_limit_bps,
            download_bps: self
                .app_state
                .avg_download_history
                .last()
                .copied()
                .unwrap_or(0),
            disk_write_completed_bps: self.app_state.avg_disk_write_completed_bps,
            recv_to_write_p95: self.app_state.recv_to_write_p95,
        };

        match self.disk_write_download_throttle.update(sample) {
            DiskBackpressureDecision::Disabled => {
                self.app_state.effective_download_limit_bps = effective_download_limit_bps(
                    self.client_configs.global_download_limit_bps,
                    None,
                );
                self.global_dl_bucket
                    .set_rate_preserving_tokens(configured_download_bucket_rate(
                        self.client_configs.global_download_limit_bps,
                    ));
            }
            DiskBackpressureDecision::Limited {
                rate_bytes_per_sec,
                capacity_bytes,
            } => {
                let adaptive_limit_bps = bytes_per_sec_to_bps(rate_bytes_per_sec);
                self.app_state.effective_download_limit_bps = effective_download_limit_bps(
                    self.client_configs.global_download_limit_bps,
                    Some(adaptive_limit_bps),
                );
                self.global_dl_bucket
                    .set_rate_with_capacity_preserving_tokens(rate_bytes_per_sec, capacity_bytes);
            }
        }
    }

    pub(super) async fn tuning_resource_limits(&mut self) {
        if self.peer_admission_stress_active() {
            tracing_event!(
                Level::DEBUG,
                base_peer_limit = self.app_state.limits.max_connected_peers,
                effective_peer_limit = self.effective_resource_limits().max_connected_peers,
                "Self-Tune: paused while wake-lag peer throttle is active"
            );
            self.apply_effective_resource_limits().await;
            return;
        }

        let history = if !self.app_state.is_seeding {
            &self.app_state.avg_download_history
        } else {
            &self.app_state.avg_upload_history
        };

        let lookback = self.tuning_controller.lookback_secs();
        let relevant_history = &history[history.len().saturating_sub(lookback)..];
        let evaluation = self.tuning_controller.evaluate_cycle(
            &self.app_state.limits,
            relevant_history,
            self.app_state.global_disk_thrash_score,
            self.app_state.adaptive_max_scpb,
        );
        self.sync_tuning_state_from_controller();

        if evaluation.accepted_improvement {
            tracing_event!(
                Level::DEBUG,
                "Self-Tune: SUCCESS. New best score: {} (raw: {}, penalty: {:.2}x)",
                evaluation.new_score,
                evaluation.new_raw_score,
                evaluation.penalty_factor
            );
        } else {
            self.app_state.limits = evaluation.effective_limits.clone();
            if evaluation.reality_check_applied {
                tracing_event!(Level::DEBUG, "Self-Tune: REALITY CHECK. Score {} (raw: {}) failed. Old best {} is stale vs. baseline {}. Resetting best to baseline.", evaluation.new_score, evaluation.new_raw_score, evaluation.best_score_before, evaluation.baseline_u64);
            } else {
                tracing_event!(Level::DEBUG, "Self-Tune: REVERTING. Score {} (raw: {}, penalty: {:.2}x) was not better than {}. (Baseline is {})", evaluation.new_score, evaluation.new_raw_score, evaluation.penalty_factor, evaluation.best_score_before, evaluation.baseline_u64);
            }

            self.apply_effective_resource_limits().await;
        }

        let (next_limits, desc) =
            make_random_adjustment(self.app_state.limits.clone(), self.app_state.is_seeding);
        self.app_state.limits = next_limits;

        tracing_event!(Level::DEBUG, "Self-Tune: Trying next change... {}", desc);
        self.apply_effective_resource_limits().await;
    }

    pub(super) fn reschedule_tuning_deadline(&mut self) {
        self.next_tuning_at =
            time::Instant::now() + Duration::from_secs(self.tuning_controller.cadence_secs());
    }

    pub(super) fn reset_tuning_for_objective_change(&mut self) {
        self.app_state.limits =
            normalize_limits_for_mode(&self.app_state.limits, self.app_state.is_seeding);
        self.tuning_controller
            .reset_for_objective_change(&self.app_state.limits);
        self.sync_tuning_state_from_controller();
        self.reschedule_tuning_deadline();
    }

    pub(super) fn sync_tuning_state_from_controller(&mut self) {
        let state = self.tuning_controller.state();
        self.app_state.last_tuning_score = state.last_tuning_score;
        self.app_state.current_tuning_score = state.current_tuning_score;
        self.app_state.last_tuning_limits = state.last_tuning_limits.clone();
        self.app_state.baseline_speed_ema = state.baseline_speed_ema;
        self.app_state.tuning_countdown = self.tuning_controller.countdown_secs();
    }
}

pub(super) fn sample_system_telemetry(
    sys: &mut System,
    previous: SystemTelemetrySnapshot,
) -> Option<SystemTelemetrySnapshot> {
    let pid = match sysinfo::get_current_pid() {
        Ok(pid) => pid,
        Err(error) => {
            tracing_event!(Level::ERROR, "Could not get current PID: {}", error);
            return None;
        }
    };

    sys.refresh_cpu_usage();
    sys.refresh_memory();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);

    Some(
        sys.process(pid)
            .map_or(previous, |process| SystemTelemetrySnapshot {
                cpu_usage: process.cpu_usage() / sys.cpus().len() as f32,
                ram_usage_percent: (process.memory() as f32 / sys.total_memory() as f32) * 100.0,
                app_ram_usage: process.memory(),
                run_time: process.run_time(),
            }),
    )
}

pub(super) fn calculate_adaptive_limits(
    client_configs: &Settings,
) -> (CalculatedLimits, Option<String>) {
    let effective_limit;
    let mut system_warning = None;
    const RECOMMENDED_MINIMUM: usize = 1024;

    if let Some(override_val) = client_configs.resource_limit_override {
        effective_limit = override_val;
        if effective_limit < RECOMMENDED_MINIMUM {
            system_warning = Some(format!(
                "Warning: Resource limit is set to {}. Performance may be degraded. Consider increasing with 'ulimit -n 65536'.",
                effective_limit
            ));
        }
    } else {
        #[cfg(unix)]
        {
            if let Ok((soft_limit, _)) = Resource::NOFILE.get() {
                effective_limit = soft_limit as usize;
                if effective_limit < RECOMMENDED_MINIMUM {
                    system_warning = Some(format!(
                        "Warning: System file handle limit is {}. Consider increasing with 'ulimit -n 65536'.",
                        effective_limit
                    ));
                }
            } else {
                effective_limit = RECOMMENDED_MINIMUM;
            }
        }
        #[cfg(windows)]
        {
            effective_limit = 8192;
        }
        #[cfg(not(any(unix, windows)))]
        {
            effective_limit = RECOMMENDED_MINIMUM;
        }
    }

    if let Some(warning) = &system_warning {
        tracing_event!(Level::WARN, "{}", warning);
    }

    let available_budget_after_reservation = effective_limit.saturating_sub(FILE_HANDLE_MINIMUM);
    let safe_budget = available_budget_after_reservation as f64 * SAFE_BUDGET_PERCENTAGE;
    const PEER_PROPORTION: f64 = 0.70;
    const DISK_READ_PROPORTION: f64 = 0.15;
    const DISK_WRITE_PROPORTION: f64 = 0.15;

    let limits = CalculatedLimits {
        reserve_permits: 0,
        max_connected_peers: (safe_budget * PEER_PROPORTION).max(10.0) as usize,
        disk_read_permits: (safe_budget * DISK_READ_PROPORTION).max(4.0) as usize,
        disk_write_permits: (safe_budget * DISK_WRITE_PROPORTION).max(4.0) as usize,
    };

    (limits, system_warning)
}
