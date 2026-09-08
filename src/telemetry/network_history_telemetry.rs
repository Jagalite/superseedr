// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::{AppState, GraphDisplayMode};
use crate::persistence::network_history::{
    enforce_retention_caps, NetworkHistoryPersistedState, NetworkHistoryPoint,
    NetworkHistoryRollupState, NetworkHistoryTiers, HOUR_1H_CAP, MINUTE_15M_CAP, MINUTE_1M_CAP,
    SECOND_1S_CAP,
};
use crate::telemetry::restore_densify::densify_points_for_restore;
use std::collections::VecDeque;
use web_time::{SystemTime, UNIX_EPOCH};

pub struct NetworkHistoryTelemetry;

const AUTO_GRAPH_EVALUATION_INTERVAL_SECS: u64 = 5;
const AUTO_GRAPH_MINIMUM_DWELL_SECS: u64 = 20;

impl NetworkHistoryTelemetry {
    pub fn on_second_tick(app_state: &mut AppState) {
        Self::on_second_tick_at(app_state, current_unix_time());
    }

    pub(crate) fn on_second_tick_at(app_state: &mut AppState, now_unix: u64) {
        let download_bps = app_state.avg_download_history.last().copied().unwrap_or(0);
        let upload_bps = app_state.avg_upload_history.last().copied().unwrap_or(0);
        let backoff_ms_max = app_state
            .disk_backoff_history_ms
            .back()
            .copied()
            .unwrap_or(0);
        if app_state.network_history_rollups.ingest_second_sample(
            &mut app_state.network_history_state,
            now_unix,
            download_bps,
            upload_bps,
            backoff_ms_max,
        ) {
            app_state.network_history_dirty = true;
        }
        app_state
            .auto_graph_activity
            .observe(now_unix, download_bps.saturating_add(upload_bps));
        update_auto_graph_window(app_state, now_unix);
    }

    pub fn apply_loaded_state(app_state: &mut AppState, state: NetworkHistoryPersistedState) {
        Self::apply_loaded_state_at(app_state, state, current_unix_time());
    }

    fn apply_loaded_state_at(
        app_state: &mut AppState,
        state: NetworkHistoryPersistedState,
        now_unix: u64,
    ) {
        let was_dirty = app_state.network_history_dirty;
        let (merged, rollups) =
            merge_state_for_late_restore(&app_state.network_history_state, state);
        let densified = densify_state_for_restore(merged, now_unix);

        app_state.avg_download_history = densified
            .tiers
            .second_1s
            .iter()
            .map(|p| p.download_bps)
            .collect();
        app_state.avg_upload_history = densified
            .tiers
            .second_1s
            .iter()
            .map(|p| p.upload_bps)
            .collect();
        app_state.disk_backoff_history_ms = VecDeque::from(
            densified
                .tiers
                .second_1s
                .iter()
                .map(|p| p.backoff_ms_max)
                .collect::<Vec<_>>(),
        );

        app_state.minute_avg_dl_history = densified
            .tiers
            .minute_1m
            .iter()
            .map(|p| p.download_bps)
            .collect();
        app_state.minute_avg_ul_history = densified
            .tiers
            .minute_1m
            .iter()
            .map(|p| p.upload_bps)
            .collect();
        app_state.minute_disk_backoff_history_ms = VecDeque::from(
            densified
                .tiers
                .minute_1m
                .iter()
                .map(|p| p.backoff_ms_max)
                .collect::<Vec<_>>(),
        );

        app_state.network_history_state = densified;
        app_state.network_history_rollups = rollups;
        // Preserve dirty state if live samples were already pending flush.
        app_state.network_history_dirty = was_dirty;
    }
}

fn update_auto_graph_window(app_state: &mut AppState, now_unix: u64) {
    if app_state.graph_mode != GraphDisplayMode::Auto
        || (app_state.auto_graph_window.last_evaluation_unix != 0
            && now_unix >= app_state.auto_graph_window.last_evaluation_unix
            && now_unix.saturating_sub(app_state.auto_graph_window.last_evaluation_unix)
                < AUTO_GRAPH_EVALUATION_INTERVAL_SECS)
    {
        return;
    }
    app_state.auto_graph_window.last_evaluation_unix = now_unix;
    let target = app_state.auto_graph_activity.mode();
    let current = app_state.auto_graph_window.effective_mode;
    if target == current {
        return;
    }

    let zooming_in = target.as_seconds() < current.as_seconds();
    let dwell_complete = app_state.auto_graph_window.last_change_unix == 0
        || now_unix < app_state.auto_graph_window.last_change_unix
        || now_unix.saturating_sub(app_state.auto_graph_window.last_change_unix)
            >= AUTO_GRAPH_MINIMUM_DWELL_SECS;
    if zooming_in || dwell_complete {
        app_state.auto_graph_window.effective_mode =
            if zooming_in { target } else { current.next() };
        app_state.auto_graph_window.last_change_unix = now_unix;
        app_state.ui.needs_redraw = true;
    }
}

fn current_unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn latest_point_timestamp(points: &[NetworkHistoryPoint]) -> u64 {
    points.last().map(|point| point.ts_unix).unwrap_or(0)
}

fn merge_state_for_late_restore(
    live_state: &NetworkHistoryPersistedState,
    loaded_state: NetworkHistoryPersistedState,
) -> (NetworkHistoryPersistedState, NetworkHistoryRollupState) {
    let mut merged = loaded_state;
    merged.schema_version = merged.schema_version.max(live_state.schema_version);
    merged.updated_at_unix = merged.updated_at_unix.max(live_state.updated_at_unix);
    let replay_cutoff_unix = latest_point_timestamp(&merged.tiers.second_1s);
    let mut rollups = NetworkHistoryRollupState::from_snapshot(&merged.rollups);

    for point in live_state
        .tiers
        .second_1s
        .iter()
        .filter(|point| point.ts_unix > replay_cutoff_unix)
    {
        let _ = rollups.ingest_second_sample(
            &mut merged,
            point.ts_unix,
            point.download_bps,
            point.upload_bps,
            point.backoff_ms_max,
        );
    }

    merged.rollups = rollups.to_snapshot();
    enforce_retention_caps(&mut merged);
    (merged, rollups)
}

fn densify_tier_points(
    points: &[NetworkHistoryPoint],
    step_secs: u64,
    max_points: usize,
    now_unix: u64,
) -> Vec<NetworkHistoryPoint> {
    densify_points_for_restore(
        points,
        step_secs,
        max_points,
        now_unix,
        |point| point.ts_unix,
        |ts_unix| NetworkHistoryPoint {
            ts_unix,
            ..Default::default()
        },
    )
}

fn densify_state_for_restore(
    state: NetworkHistoryPersistedState,
    now_unix: u64,
) -> NetworkHistoryPersistedState {
    let mut dense = NetworkHistoryPersistedState {
        schema_version: state.schema_version,
        updated_at_unix: state.updated_at_unix,
        rollups: state.rollups,
        tiers: NetworkHistoryTiers {
            second_1s: densify_tier_points(&state.tiers.second_1s, 1, SECOND_1S_CAP, now_unix),
            minute_1m: densify_tier_points(&state.tiers.minute_1m, 60, MINUTE_1M_CAP, now_unix),
            minute_15m: densify_tier_points(
                &state.tiers.minute_15m,
                15 * 60,
                MINUTE_15M_CAP,
                now_unix,
            ),
            hour_1h: densify_tier_points(&state.tiers.hour_1h, 60 * 60, HOUR_1H_CAP, now_unix),
        },
    };
    enforce_retention_caps(&mut dense);
    dense
}

#[cfg(test)]
mod tests {
    use super::{
        densify_state_for_restore, densify_tier_points, merge_state_for_late_restore,
        NetworkHistoryTelemetry,
    };
    use crate::app::{AppState, GraphDisplayMode};
    use crate::persistence::network_history::{
        NetworkHistoryPersistedState, NetworkHistoryPoint, NetworkHistoryRollupSnapshot,
        PersistedRollupAccumulator,
    };
    use std::collections::VecDeque;

    fn partial_accumulator(
        count: u32,
        dl_sum: u128,
        ul_sum: u128,
        backoff_max: u64,
    ) -> PersistedRollupAccumulator {
        PersistedRollupAccumulator {
            count,
            dl_sum,
            ul_sum,
            backoff_max,
        }
    }

    fn point(ts_unix: u64, download_bps: u64, upload_bps: u64) -> NetworkHistoryPoint {
        NetworkHistoryPoint {
            ts_unix,
            download_bps,
            upload_bps,
            backoff_ms_max: 0,
        }
    }

    fn live_tick(state: &mut AppState, now: u64, download: u64, upload: u64) {
        state.avg_download_history.clear();
        state.avg_download_history.push(download);
        state.avg_upload_history.clear();
        state.avg_upload_history.push(upload);
        NetworkHistoryTelemetry::on_second_tick_at(state, now);
    }

    #[test]
    fn auto_graph_frames_sustained_rises_with_delayed_ticks() {
        for step in [1, 2, 3] {
            let mut state = AppState::default();
            for t in (1..=1_200).step_by(step) {
                let rate = if (601..901).contains(&t) {
                    100_000_000
                } else {
                    5_000_000
                };
                live_tick(&mut state, t, rate, 0);
                if (880..1_010).contains(&t) {
                    assert_eq!(
                        state.auto_graph_window.effective_mode,
                        GraphDisplayMode::TenMinutes,
                        "{step}s ticks at {t}"
                    );
                }
                if t >= 1_040 {
                    assert_eq!(
                        state.auto_graph_window.effective_mode,
                        GraphDisplayMode::OneMinute,
                        "{step}s ticks at {t}"
                    );
                }
            }
        }
    }

    #[test]
    fn auto_graph_frames_live_download_and_upload_spikes_and_then_resets() {
        for upload in [false, true] {
            let mut state = AppState::default();
            for t in 1..=1_800 {
                let rate = if (601..=720).contains(&t) {
                    100_000_000
                } else {
                    5_000_000
                };
                let (dl, ul) = if upload { (0, rate) } else { (rate, 0) };
                live_tick(&mut state, t, dl, ul);
                assert!(state.auto_graph_window.effective_mode.as_seconds() <= 600);
                if t == 720 || t == 800 {
                    assert_eq!(
                        state.auto_graph_window.effective_mode,
                        GraphDisplayMode::FiveMinutes
                    );
                }
                if t >= 850 {
                    assert_eq!(
                        state.auto_graph_window.effective_mode,
                        GraphDisplayMode::OneMinute
                    );
                }
            }
        }
    }

    #[test]
    fn late_restore_and_unrelated_history_do_not_change_live_auto_decisions() {
        let mut clean = AppState::default();
        let mut restored = AppState::default();
        let epoch = 60 * 86_400;
        for elapsed in 1..=1_800 {
            let now = epoch + elapsed;
            let rate = if (601..=900).contains(&elapsed) {
                100_000_000
            } else {
                5_000_000
            };
            if elapsed == 750 {
                let before = restored.auto_graph_activity;
                let mut loaded = NetworkHistoryPersistedState::default();
                loaded.tiers.second_1s = vec![point(epoch - 3_600, 100_000_000, 0)];
                loaded.tiers.minute_1m = vec![point(epoch - 86_400, 100_000_000, 0)];
                loaded.tiers.minute_15m = vec![point(epoch - 14 * 86_400, 100_000_000, 0)];
                NetworkHistoryTelemetry::apply_loaded_state_at(&mut restored, loaded, now);
                assert_eq!(restored.auto_graph_activity, before);
            }
            live_tick(&mut clean, now, rate, 0);
            live_tick(&mut restored, now, rate, 0);
            assert_eq!(clean.auto_graph_activity, restored.auto_graph_activity);
            assert_eq!(clean.auto_graph_window, restored.auto_graph_window);
        }
    }

    #[test]
    fn manual_ranges_keep_tracking_activity_for_return_to_auto() {
        let mut state = AppState {
            graph_mode: GraphDisplayMode::SevenDays,
            ..Default::default()
        };
        let previous_window = state.auto_graph_window;
        for t in 1..=720 {
            live_tick(
                &mut state,
                t,
                if t > 600 { 100_000_000 } else { 5_000_000 },
                0,
            );
            assert_eq!(state.graph_mode, GraphDisplayMode::SevenDays);
            assert_eq!(state.auto_graph_window, previous_window);
        }
        assert_eq!(
            state.auto_graph_activity.mode(),
            GraphDisplayMode::FiveMinutes
        );
        state.graph_mode = GraphDisplayMode::Auto;
        state.auto_graph_window = Default::default();
        live_tick(&mut state, 721, 100_000_000, 0);
        assert_eq!(
            state.auto_graph_window.effective_mode,
            GraphDisplayMode::FiveMinutes
        );
    }

    #[test]
    fn clock_rollback_does_not_block_auto_evaluation() {
        let mut state = AppState::default();
        for t in 1_000..=1_900 {
            live_tick(
                &mut state,
                t,
                if t > 1_600 { 100_000_000 } else { 5_000_000 },
                0,
            );
        }
        assert_eq!(
            state.auto_graph_window.effective_mode,
            GraphDisplayMode::TenMinutes
        );
        live_tick(&mut state, 500, 5_000_000, 0);
        assert_eq!(
            state.auto_graph_window.effective_mode,
            GraphDisplayMode::OneMinute
        );
    }

    #[test]
    fn apply_loaded_state_replays_live_seconds_and_preserves_dirty() {
        let mut app_state = AppState {
            avg_download_history: vec![100],
            avg_upload_history: vec![10],
            disk_backoff_history_ms: VecDeque::from(vec![1]),
            network_history_dirty: true,
            ..Default::default()
        };
        app_state
            .network_history_state
            .tiers
            .second_1s
            .push(NetworkHistoryPoint {
                ts_unix: 2,
                download_bps: 100,
                upload_bps: 10,
                backoff_ms_max: 1,
            });
        app_state
            .network_history_state
            .tiers
            .second_1s
            .push(NetworkHistoryPoint {
                ts_unix: 3,
                download_bps: 50,
                upload_bps: 5,
                backoff_ms_max: 4,
            });

        let mut loaded = NetworkHistoryPersistedState {
            rollups: NetworkHistoryRollupSnapshot {
                second_to_minute: partial_accumulator(1, 200, 20, 2),
                ..Default::default()
            },
            ..Default::default()
        };
        loaded.tiers.second_1s.push(NetworkHistoryPoint {
            ts_unix: 1,
            download_bps: 200,
            upload_bps: 20,
            backoff_ms_max: 2,
        });

        NetworkHistoryTelemetry::apply_loaded_state_at(&mut app_state, loaded, 3);

        assert_eq!(app_state.avg_download_history, vec![200, 100, 50]);
        assert_eq!(app_state.avg_upload_history, vec![20, 10, 5]);
        assert_eq!(
            app_state.disk_backoff_history_ms,
            VecDeque::from(vec![2, 1, 4])
        );
        assert_eq!(
            app_state
                .network_history_rollups
                .to_snapshot()
                .second_to_minute,
            partial_accumulator(3, 350, 35, 4)
        );
        assert!(app_state.network_history_dirty);
    }

    #[test]
    fn merge_state_for_late_restore_replays_only_new_live_seconds() {
        let mut live = NetworkHistoryPersistedState::default();
        live.tiers.second_1s.push(NetworkHistoryPoint {
            ts_unix: 5,
            download_bps: 500,
            upload_bps: 50,
            backoff_ms_max: 5,
        });
        live.tiers.second_1s.push(NetworkHistoryPoint {
            ts_unix: 6,
            download_bps: 600,
            upload_bps: 60,
            backoff_ms_max: 6,
        });
        let mut loaded = NetworkHistoryPersistedState {
            rollups: NetworkHistoryRollupSnapshot {
                second_to_minute: partial_accumulator(1, 300, 30, 3),
                ..Default::default()
            },
            ..Default::default()
        };
        loaded.tiers.second_1s.push(NetworkHistoryPoint {
            ts_unix: 5,
            download_bps: 300,
            upload_bps: 30,
            backoff_ms_max: 3,
        });

        let (merged, rollups) = merge_state_for_late_restore(&live, loaded);
        assert_eq!(merged.tiers.second_1s.len(), 2);
        assert_eq!(merged.tiers.second_1s[0].download_bps, 300);
        assert_eq!(merged.tiers.second_1s[1].download_bps, 600);
        assert_eq!(
            rollups.to_snapshot().second_to_minute,
            partial_accumulator(2, 900, 90, 6)
        );
    }

    #[test]
    fn densify_state_for_restore_fills_sparse_second_gaps_and_tail_with_zeros() {
        let mut sparse = NetworkHistoryPersistedState::default();
        sparse.tiers.second_1s.push(NetworkHistoryPoint {
            ts_unix: 1,
            download_bps: 200,
            upload_bps: 20,
            backoff_ms_max: 2,
        });
        sparse.tiers.second_1s.push(NetworkHistoryPoint {
            ts_unix: 3,
            download_bps: 100,
            upload_bps: 10,
            backoff_ms_max: 1,
        });

        let dense = densify_state_for_restore(sparse, 4);
        assert_eq!(
            dense
                .tiers
                .second_1s
                .iter()
                .map(|p| p.download_bps)
                .collect::<Vec<_>>(),
            vec![200, 0, 100, 0]
        );
    }

    #[test]
    fn densify_state_for_restore_fills_sparse_minute_gaps_and_tail_with_zeros() {
        let mut sparse = NetworkHistoryPersistedState::default();
        sparse.tiers.minute_1m.push(NetworkHistoryPoint {
            ts_unix: 60,
            download_bps: 600,
            upload_bps: 60,
            backoff_ms_max: 3,
        });
        sparse.tiers.minute_1m.push(NetworkHistoryPoint {
            ts_unix: 180,
            download_bps: 300,
            upload_bps: 30,
            backoff_ms_max: 1,
        });

        let dense = densify_state_for_restore(sparse, 240);
        assert_eq!(
            dense
                .tiers
                .minute_1m
                .iter()
                .map(|p| p.download_bps)
                .collect::<Vec<_>>(),
            vec![600, 0, 300, 0]
        );
    }

    #[test]
    fn densify_tier_points_limits_sparse_tail_fill_to_retention_window() {
        let dense = densify_tier_points(
            &[NetworkHistoryPoint {
                ts_unix: 1,
                download_bps: 200,
                upload_bps: 20,
                backoff_ms_max: 2,
            }],
            1,
            4,
            1_000_000,
        );

        assert_eq!(
            dense.iter().map(|point| point.ts_unix).collect::<Vec<_>>(),
            vec![999_997, 999_998, 999_999, 1_000_000]
        );
        assert!(dense.iter().all(|point| point.download_bps == 0));
        assert!(dense.iter().all(|point| point.upload_bps == 0));
        assert!(dense.iter().all(|point| point.backoff_ms_max == 0));
    }

    #[test]
    fn apply_loaded_state_restores_dense_histories_from_sparse_points() {
        let mut app_state = AppState::default();
        let mut loaded = NetworkHistoryPersistedState::default();
        loaded.tiers.second_1s.push(NetworkHistoryPoint {
            ts_unix: 10,
            download_bps: 500,
            upload_bps: 50,
            backoff_ms_max: 4,
        });
        loaded.tiers.second_1s.push(NetworkHistoryPoint {
            ts_unix: 12,
            download_bps: 250,
            upload_bps: 25,
            backoff_ms_max: 2,
        });

        NetworkHistoryTelemetry::apply_loaded_state_at(&mut app_state, loaded, 13);
        assert_eq!(app_state.avg_download_history, vec![500, 0, 250, 0]);
        assert_eq!(app_state.avg_upload_history, vec![50, 0, 25, 0]);
        assert_eq!(
            app_state.disk_backoff_history_ms,
            VecDeque::from(vec![4, 0, 2, 0])
        );
    }

    #[test]
    fn densify_state_for_restore_preserves_rollup_snapshot() {
        let sparse = NetworkHistoryPersistedState {
            rollups: NetworkHistoryRollupSnapshot {
                second_to_minute: partial_accumulator(9, 900, 90, 7),
                ..Default::default()
            },
            tiers: crate::persistence::network_history::NetworkHistoryTiers {
                second_1s: vec![NetworkHistoryPoint {
                    ts_unix: 10,
                    download_bps: 500,
                    upload_bps: 50,
                    backoff_ms_max: 4,
                }],
                ..Default::default()
            },
            ..Default::default()
        };

        let dense = densify_state_for_restore(sparse.clone(), 12);
        assert_eq!(dense.rollups, sparse.rollups);
    }

    #[test]
    fn apply_loaded_state_restores_second_to_minute_rollup_from_snapshot_without_parent_boundary() {
        let mut app_state = AppState::default();
        let mut loaded = NetworkHistoryPersistedState {
            updated_at_unix: 59,
            rollups: NetworkHistoryRollupSnapshot {
                second_to_minute: partial_accumulator(59, 590, 59, 1),
                ..Default::default()
            },
            ..Default::default()
        };
        loaded.tiers.second_1s.push(NetworkHistoryPoint {
            ts_unix: 59,
            download_bps: 10,
            upload_bps: 1,
            backoff_ms_max: 1,
        });

        NetworkHistoryTelemetry::apply_loaded_state_at(&mut app_state, loaded, 59);

        assert!(app_state.network_history_rollups.ingest_second_sample(
            &mut app_state.network_history_state,
            60,
            70,
            7,
            9,
        ));
        assert_eq!(app_state.network_history_state.tiers.minute_1m.len(), 1);
        assert_eq!(
            app_state.network_history_state.tiers.minute_1m[0].download_bps,
            11
        );
        assert_eq!(
            app_state.network_history_state.tiers.minute_1m[0].upload_bps,
            1
        );
        assert_eq!(
            app_state.network_history_state.tiers.minute_1m[0].backoff_ms_max,
            9
        );
    }

    #[test]
    fn apply_loaded_state_restores_minute_to_15m_rollup_from_snapshot_without_parent_boundary() {
        let mut app_state = AppState::default();
        let mut loaded = NetworkHistoryPersistedState {
            updated_at_unix: 14 * 60,
            rollups: NetworkHistoryRollupSnapshot {
                minute_to_15m: partial_accumulator(14, 140, 28, 3),
                ..Default::default()
            },
            ..Default::default()
        };
        loaded.tiers.minute_1m.push(NetworkHistoryPoint {
            ts_unix: 14 * 60,
            download_bps: 10,
            upload_bps: 2,
            backoff_ms_max: 3,
        });

        NetworkHistoryTelemetry::apply_loaded_state_at(&mut app_state, loaded, 14 * 60);

        for ts in (14 * 60 + 1)..=(15 * 60) {
            assert!(app_state.network_history_rollups.ingest_second_sample(
                &mut app_state.network_history_state,
                ts,
                40,
                4,
                5,
            ));
        }

        assert_eq!(app_state.network_history_state.tiers.minute_15m.len(), 1);
        assert_eq!(
            app_state.network_history_state.tiers.minute_15m[0].download_bps,
            12
        );
        assert_eq!(
            app_state.network_history_state.tiers.minute_15m[0].upload_bps,
            2
        );
        assert_eq!(
            app_state.network_history_state.tiers.minute_15m[0].backoff_ms_max,
            5
        );
    }

    #[test]
    fn apply_loaded_state_restores_15m_to_hour_rollup_from_snapshot_without_parent_boundary() {
        let mut app_state = AppState::default();
        let mut loaded = NetworkHistoryPersistedState {
            updated_at_unix: 3 * 15 * 60,
            rollups: NetworkHistoryRollupSnapshot {
                m15_to_hour: partial_accumulator(3, 60, 9, 4),
                ..Default::default()
            },
            ..Default::default()
        };
        loaded.tiers.minute_15m.push(NetworkHistoryPoint {
            ts_unix: 3 * 15 * 60,
            download_bps: 20,
            upload_bps: 3,
            backoff_ms_max: 4,
        });

        NetworkHistoryTelemetry::apply_loaded_state_at(&mut app_state, loaded, 3 * 15 * 60);

        for ts in (3 * 15 * 60 + 1)..=(4 * 15 * 60) {
            assert!(app_state.network_history_rollups.ingest_second_sample(
                &mut app_state.network_history_state,
                ts,
                80,
                8,
                9,
            ));
        }

        assert_eq!(app_state.network_history_state.tiers.hour_1h.len(), 1);
        assert_eq!(
            app_state.network_history_state.tiers.hour_1h[0].download_bps,
            35
        );
        assert_eq!(
            app_state.network_history_state.tiers.hour_1h[0].upload_bps,
            4
        );
        assert_eq!(
            app_state.network_history_state.tiers.hour_1h[0].backoff_ms_max,
            9
        );
    }
}
