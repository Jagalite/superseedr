// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::AppState;
use crate::persistence::activity_history::{
    enforce_retention_caps, retain_only_torrent_series_for_keys, ActivityHistoryPersistedState,
    ActivityHistoryPoint, ActivityHistorySeries, ActivityHistorySeriesRollupState,
    ActivityHistoryTiers,
};
use crate::persistence::network_history::{
    HOUR_1H_CAP, MINUTE_15M_CAP, MINUTE_1M_CAP, SECOND_1S_CAP,
};
use crate::telemetry::restore_densify::densify_points_for_restore;
use std::collections::HashSet;
use web_time::{SystemTime, UNIX_EPOCH};

pub struct ActivityHistoryTelemetry;

impl ActivityHistoryTelemetry {
    pub fn on_second_tick(app_state: &mut AppState) {
        Self::on_second_tick_at(app_state, current_unix_time());
    }

    pub(crate) fn on_second_tick_at(app_state: &mut AppState, now_unix: u64) {
        let active_torrent_keys: HashSet<String> =
            app_state.torrents.keys().map(hex::encode).collect();
        let torrent_samples: Vec<(String, u64, u64)> = app_state
            .torrents
            .iter()
            .map(|(info_hash, torrent)| {
                (
                    hex::encode(info_hash),
                    torrent.smoothed_download_speed_bps,
                    torrent.smoothed_upload_speed_bps,
                )
            })
            .collect();

        retain_only_torrent_series_for_keys(
            &mut app_state.activity_history_state,
            &mut app_state.activity_history_rollups,
            &active_torrent_keys,
        );

        let cpu_x10 = (app_state.cpu_usage.clamp(0.0, 100.0) * 10.0).round() as u64;
        let ram_x10 = (app_state.ram_usage_percent.clamp(0.0, 100.0) * 10.0).round() as u64;
        let tuning_current = app_state.current_tuning_score;
        let tuning_best = app_state.last_tuning_score;

        let mut changed = false;
        changed |= app_state.activity_history_rollups.cpu.ingest_second_sample(
            &mut app_state.activity_history_state.cpu,
            now_unix,
            cpu_x10,
            0,
        );
        changed |= app_state.activity_history_rollups.ram.ingest_second_sample(
            &mut app_state.activity_history_state.ram,
            now_unix,
            ram_x10,
            0,
        );
        changed |= app_state
            .activity_history_rollups
            .disk
            .ingest_second_sample(
                &mut app_state.activity_history_state.disk,
                now_unix,
                app_state.avg_disk_read_bps,
                app_state.avg_disk_write_bps,
            );
        changed |= app_state
            .activity_history_rollups
            .tuning
            .ingest_second_sample(
                &mut app_state.activity_history_state.tuning,
                now_unix,
                tuning_current,
                tuning_best,
            );

        for (key, dl_bps, ul_bps) in torrent_samples {
            let series = app_state
                .activity_history_state
                .torrents
                .entry(key.clone())
                .or_default();
            let rollups = app_state
                .activity_history_rollups
                .torrents
                .entry(key)
                .or_default();
            changed |= rollups.ingest_second_sample(series, now_unix, dl_bps, ul_bps);
        }

        if changed {
            app_state.activity_history_dirty = true;
        }

        enforce_retention_caps(&mut app_state.activity_history_state);
    }

    pub fn apply_loaded_state(app_state: &mut AppState, state: ActivityHistoryPersistedState) {
        Self::apply_loaded_state_at(app_state, state, current_unix_time());
    }

    fn apply_loaded_state_at(
        app_state: &mut AppState,
        state: ActivityHistoryPersistedState,
        now_unix: u64,
    ) {
        let was_dirty = app_state.activity_history_dirty;
        let merged = merge_state_for_late_restore(&app_state.activity_history_state, state);
        let densified = densify_state_for_restore(merged, now_unix);
        app_state.activity_history_state = densified;
        app_state.activity_history_rollups =
            crate::persistence::activity_history::ActivityHistoryRollupState::from_persisted(
                &app_state.activity_history_state,
            );
        app_state.activity_history_dirty = was_dirty;
    }
}

fn current_unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn latest_second_timestamp(series: &ActivityHistorySeries) -> u64 {
    series
        .tiers
        .second_1s
        .last()
        .map(|point| point.ts_unix)
        .unwrap_or(0)
}

fn replay_live_seconds_into_loaded(
    live_series: &ActivityHistorySeries,
    merged_series: &mut ActivityHistorySeries,
) {
    let replay_cutoff_unix = latest_second_timestamp(merged_series);
    let mut rollups = ActivityHistorySeriesRollupState::from_snapshot(&merged_series.rollups);

    for point in live_series
        .tiers
        .second_1s
        .iter()
        .filter(|point| point.ts_unix > replay_cutoff_unix)
    {
        let _ = rollups.ingest_second_sample(
            merged_series,
            point.ts_unix,
            point.primary,
            point.secondary,
        );
    }
}

fn merge_state_for_late_restore(
    live_state: &ActivityHistoryPersistedState,
    loaded_state: ActivityHistoryPersistedState,
) -> ActivityHistoryPersistedState {
    let mut merged = loaded_state;
    merged.schema_version = merged.schema_version.max(live_state.schema_version);
    merged.updated_at_unix = merged.updated_at_unix.max(live_state.updated_at_unix);

    replay_live_seconds_into_loaded(&live_state.cpu, &mut merged.cpu);
    replay_live_seconds_into_loaded(&live_state.ram, &mut merged.ram);
    replay_live_seconds_into_loaded(&live_state.disk, &mut merged.disk);
    replay_live_seconds_into_loaded(&live_state.tuning, &mut merged.tuning);

    let mut all_torrents: HashSet<String> = merged.torrents.keys().cloned().collect();
    all_torrents.extend(live_state.torrents.keys().cloned());

    for info_hash in all_torrents {
        if let Some(live_series) = live_state.torrents.get(&info_hash) {
            let merged_series = merged.torrents.entry(info_hash).or_default();
            replay_live_seconds_into_loaded(live_series, merged_series);
        }
    }

    enforce_retention_caps(&mut merged);
    merged
}

fn densify_tier_points(
    points: &[ActivityHistoryPoint],
    step_secs: u64,
    max_points: usize,
    now_unix: u64,
) -> Vec<ActivityHistoryPoint> {
    densify_points_for_restore(
        points,
        step_secs,
        max_points,
        now_unix,
        |point| point.ts_unix,
        |ts_unix| ActivityHistoryPoint {
            ts_unix,
            ..Default::default()
        },
    )
}

fn densify_series_for_restore(
    series: &ActivityHistorySeries,
    now_unix: u64,
) -> ActivityHistorySeries {
    ActivityHistorySeries {
        rollups: series.rollups.clone(),
        tiers: ActivityHistoryTiers {
            second_1s: densify_tier_points(&series.tiers.second_1s, 1, SECOND_1S_CAP, now_unix),
            minute_1m: densify_tier_points(&series.tiers.minute_1m, 60, MINUTE_1M_CAP, now_unix),
            minute_15m: densify_tier_points(
                &series.tiers.minute_15m,
                15 * 60,
                MINUTE_15M_CAP,
                now_unix,
            ),
            hour_1h: densify_tier_points(&series.tiers.hour_1h, 60 * 60, HOUR_1H_CAP, now_unix),
        },
    }
}

fn densify_state_for_restore(
    state: ActivityHistoryPersistedState,
    now_unix: u64,
) -> ActivityHistoryPersistedState {
    let mut dense = ActivityHistoryPersistedState {
        schema_version: state.schema_version,
        updated_at_unix: state.updated_at_unix,
        cpu: densify_series_for_restore(&state.cpu, now_unix),
        ram: densify_series_for_restore(&state.ram, now_unix),
        disk: densify_series_for_restore(&state.disk, now_unix),
        tuning: densify_series_for_restore(&state.tuning, now_unix),
        torrents: state
            .torrents
            .iter()
            .map(|(info_hash, series)| {
                (
                    info_hash.clone(),
                    densify_series_for_restore(series, now_unix),
                )
            })
            .collect(),
    };
    enforce_retention_caps(&mut dense);
    dense
}

#[cfg(test)]
mod tests {
    use super::{
        densify_state_for_restore, densify_tier_points, merge_state_for_late_restore,
        ActivityHistoryTelemetry,
    };
    use crate::app::{AppState, TorrentDisplayState};
    use crate::persistence::activity_history::{
        ActivityHistoryPersistedState, ActivityHistoryPoint, ActivityHistoryRollupSnapshot,
        ActivityHistorySeries, ActivityHistoryTiers, PersistedRollupAccumulator,
    };

    fn partial_accumulator(
        count: u32,
        primary_sum: u128,
        secondary_sum: u128,
    ) -> PersistedRollupAccumulator {
        PersistedRollupAccumulator {
            count,
            primary_sum,
            secondary_sum,
        }
    }

    #[test]
    fn apply_loaded_state_replays_live_seconds_and_preserves_dirty() {
        let mut app_state = AppState {
            activity_history_dirty: true,
            ..Default::default()
        };
        app_state
            .activity_history_state
            .cpu
            .tiers
            .second_1s
            .push(ActivityHistoryPoint {
                ts_unix: 5,
                primary: 500,
                secondary: 50,
            });
        app_state
            .activity_history_state
            .cpu
            .tiers
            .second_1s
            .push(ActivityHistoryPoint {
                ts_unix: 6,
                primary: 600,
                secondary: 60,
            });

        let mut loaded = ActivityHistoryPersistedState {
            cpu: ActivityHistorySeries {
                rollups: ActivityHistoryRollupSnapshot {
                    second_to_minute: partial_accumulator(1, 300, 30),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        loaded.cpu.tiers.second_1s.push(ActivityHistoryPoint {
            ts_unix: 5,
            primary: 300,
            secondary: 30,
        });

        ActivityHistoryTelemetry::apply_loaded_state_at(&mut app_state, loaded, 6);

        assert_eq!(
            app_state
                .activity_history_state
                .cpu
                .tiers
                .second_1s
                .iter()
                .map(|point| point.primary)
                .collect::<Vec<_>>(),
            vec![300, 600]
        );
        assert_eq!(
            app_state
                .activity_history_rollups
                .cpu
                .to_snapshot()
                .second_to_minute,
            partial_accumulator(2, 900, 90)
        );
        assert!(app_state.activity_history_dirty);
    }

    #[test]
    fn merge_state_for_late_restore_replays_only_new_live_seconds() {
        let mut live = ActivityHistoryPersistedState::default();
        live.cpu.tiers.second_1s.push(ActivityHistoryPoint {
            ts_unix: 5,
            primary: 500,
            secondary: 50,
        });
        live.cpu.tiers.second_1s.push(ActivityHistoryPoint {
            ts_unix: 6,
            primary: 600,
            secondary: 60,
        });
        let mut loaded = ActivityHistoryPersistedState {
            cpu: ActivityHistorySeries {
                rollups: ActivityHistoryRollupSnapshot {
                    second_to_minute: partial_accumulator(1, 300, 30),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        loaded.cpu.tiers.second_1s.push(ActivityHistoryPoint {
            ts_unix: 5,
            primary: 300,
            secondary: 30,
        });

        let merged = merge_state_for_late_restore(&live, loaded);

        assert_eq!(merged.cpu.tiers.second_1s.len(), 2);
        assert_eq!(merged.cpu.tiers.second_1s[0].primary, 300);
        assert_eq!(merged.cpu.tiers.second_1s[1].primary, 600);
        assert_eq!(
            merged.cpu.rollups.second_to_minute,
            partial_accumulator(2, 900, 90)
        );
    }

    #[test]
    fn densify_state_for_restore_fills_sparse_second_gaps_and_tail_with_zeros() {
        let mut sparse = ActivityHistoryPersistedState::default();
        sparse.cpu.tiers.second_1s.push(ActivityHistoryPoint {
            ts_unix: 1,
            primary: 200,
            secondary: 20,
        });
        sparse.cpu.tiers.second_1s.push(ActivityHistoryPoint {
            ts_unix: 3,
            primary: 100,
            secondary: 10,
        });

        let dense = densify_state_for_restore(sparse, 4);
        assert_eq!(
            dense
                .cpu
                .tiers
                .second_1s
                .iter()
                .map(|point| point.primary)
                .collect::<Vec<_>>(),
            vec![200, 0, 100, 0]
        );
    }

    #[test]
    fn densify_state_for_restore_fills_sparse_torrent_gaps_and_tail_with_zeros() {
        let mut sparse = ActivityHistoryPersistedState::default();
        sparse
            .torrents
            .entry("deadbeef".to_owned())
            .or_default()
            .tiers
            .minute_1m
            .push(ActivityHistoryPoint {
                ts_unix: 60,
                primary: 600,
                secondary: 60,
            });
        sparse
            .torrents
            .entry("deadbeef".to_owned())
            .or_default()
            .tiers
            .minute_1m
            .push(ActivityHistoryPoint {
                ts_unix: 180,
                primary: 300,
                secondary: 30,
            });

        let dense = densify_state_for_restore(sparse, 240);
        assert_eq!(
            dense.torrents["deadbeef"]
                .tiers
                .minute_1m
                .iter()
                .map(|point| point.primary)
                .collect::<Vec<_>>(),
            vec![600, 0, 300, 0]
        );
    }

    #[test]
    fn densify_tier_points_limits_sparse_tail_fill_to_retention_window() {
        let dense = densify_tier_points(
            &[ActivityHistoryPoint {
                ts_unix: 1,
                primary: 200,
                secondary: 20,
            }],
            1,
            4,
            1_000_000,
        );

        assert_eq!(
            dense.iter().map(|point| point.ts_unix).collect::<Vec<_>>(),
            vec![999_997, 999_998, 999_999, 1_000_000]
        );
        assert!(dense.iter().all(|point| point.primary == 0));
        assert!(dense.iter().all(|point| point.secondary == 0));
    }

    #[test]
    fn apply_loaded_state_restores_dense_series_from_sparse_points() {
        let mut app_state = AppState::default();
        let mut loaded = ActivityHistoryPersistedState::default();
        loaded.cpu.tiers.second_1s.push(ActivityHistoryPoint {
            ts_unix: 10,
            primary: 500,
            secondary: 50,
        });
        loaded.cpu.tiers.second_1s.push(ActivityHistoryPoint {
            ts_unix: 12,
            primary: 250,
            secondary: 25,
        });

        ActivityHistoryTelemetry::apply_loaded_state_at(&mut app_state, loaded, 13);
        assert_eq!(
            app_state
                .activity_history_state
                .cpu
                .tiers
                .second_1s
                .iter()
                .map(|point| point.primary)
                .collect::<Vec<_>>(),
            vec![500, 0, 250, 0]
        );
        assert_eq!(
            app_state
                .activity_history_state
                .cpu
                .tiers
                .second_1s
                .iter()
                .map(|point| point.secondary)
                .collect::<Vec<_>>(),
            vec![50, 0, 25, 0]
        );
    }

    #[test]
    fn densify_state_for_restore_preserves_rollup_snapshot() {
        let sparse = ActivityHistoryPersistedState {
            cpu: ActivityHistorySeries {
                rollups: ActivityHistoryRollupSnapshot {
                    second_to_minute: partial_accumulator(9, 900, 90),
                    ..Default::default()
                },
                tiers: ActivityHistoryTiers {
                    second_1s: vec![ActivityHistoryPoint {
                        ts_unix: 10,
                        primary: 500,
                        secondary: 50,
                    }],
                    ..Default::default()
                },
            },
            ..Default::default()
        };

        let dense = densify_state_for_restore(sparse.clone(), 12);
        assert_eq!(dense.cpu.rollups, sparse.cpu.rollups);
    }

    #[test]
    fn apply_loaded_state_restores_second_to_minute_rollup_from_snapshot_without_parent_boundary() {
        let mut app_state = AppState::default();
        let mut loaded = ActivityHistoryPersistedState {
            updated_at_unix: 59,
            cpu: ActivityHistorySeries {
                rollups: ActivityHistoryRollupSnapshot {
                    second_to_minute: partial_accumulator(59, 590, 59),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        loaded.cpu.tiers.second_1s.push(ActivityHistoryPoint {
            ts_unix: 59,
            primary: 10,
            secondary: 1,
        });

        ActivityHistoryTelemetry::apply_loaded_state_at(&mut app_state, loaded, 59);

        assert!(app_state.activity_history_rollups.cpu.ingest_second_sample(
            &mut app_state.activity_history_state.cpu,
            60,
            70,
            7,
        ));
        assert_eq!(
            app_state.activity_history_state.cpu.tiers.minute_1m.len(),
            1
        );
        assert_eq!(
            app_state.activity_history_state.cpu.tiers.minute_1m[0].primary,
            11
        );
        assert_eq!(
            app_state.activity_history_state.cpu.tiers.minute_1m[0].secondary,
            1
        );
    }

    #[test]
    fn apply_loaded_state_restores_minute_to_15m_rollup_from_snapshot_without_parent_boundary() {
        let mut app_state = AppState::default();
        let mut loaded = ActivityHistoryPersistedState {
            updated_at_unix: 14 * 60,
            cpu: ActivityHistorySeries {
                rollups: ActivityHistoryRollupSnapshot {
                    minute_to_15m: partial_accumulator(14, 140, 28),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        loaded.cpu.tiers.minute_1m.push(ActivityHistoryPoint {
            ts_unix: 14 * 60,
            primary: 10,
            secondary: 2,
        });

        ActivityHistoryTelemetry::apply_loaded_state_at(&mut app_state, loaded, 14 * 60);

        for ts in (14 * 60 + 1)..=(15 * 60) {
            assert!(app_state.activity_history_rollups.cpu.ingest_second_sample(
                &mut app_state.activity_history_state.cpu,
                ts,
                40,
                4,
            ));
        }

        assert_eq!(
            app_state.activity_history_state.cpu.tiers.minute_15m.len(),
            1
        );
        assert_eq!(
            app_state.activity_history_state.cpu.tiers.minute_15m[0].primary,
            12
        );
        assert_eq!(
            app_state.activity_history_state.cpu.tiers.minute_15m[0].secondary,
            2
        );
    }

    #[test]
    fn apply_loaded_state_restores_15m_to_hour_rollup_from_snapshot_without_parent_boundary() {
        let mut app_state = AppState::default();
        let mut loaded = ActivityHistoryPersistedState {
            updated_at_unix: 3 * 15 * 60,
            cpu: ActivityHistorySeries {
                rollups: ActivityHistoryRollupSnapshot {
                    m15_to_hour: partial_accumulator(3, 60, 9),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        loaded.cpu.tiers.minute_15m.push(ActivityHistoryPoint {
            ts_unix: 3 * 15 * 60,
            primary: 20,
            secondary: 3,
        });

        ActivityHistoryTelemetry::apply_loaded_state_at(&mut app_state, loaded, 3 * 15 * 60);

        for ts in (3 * 15 * 60 + 1)..=(4 * 15 * 60) {
            assert!(app_state.activity_history_rollups.cpu.ingest_second_sample(
                &mut app_state.activity_history_state.cpu,
                ts,
                80,
                8,
            ));
        }

        assert_eq!(app_state.activity_history_state.cpu.tiers.hour_1h.len(), 1);
        assert_eq!(
            app_state.activity_history_state.cpu.tiers.hour_1h[0].primary,
            35
        );
        assert_eq!(
            app_state.activity_history_state.cpu.tiers.hour_1h[0].secondary,
            4
        );
    }

    #[test]
    fn second_tick_keeps_hidden_torrent_history_when_ui_filter_is_active() {
        let mut app_state = AppState::default();
        let visible_hash = vec![1; 20];
        let hidden_hash = vec![2; 20];
        let hidden_key = hex::encode(&hidden_hash);

        let visible = TorrentDisplayState {
            smoothed_download_speed_bps: 10,
            smoothed_upload_speed_bps: 5,
            ..Default::default()
        };
        app_state.torrents.insert(visible_hash.clone(), visible);

        let hidden = TorrentDisplayState {
            smoothed_download_speed_bps: 20,
            smoothed_upload_speed_bps: 8,
            ..Default::default()
        };
        app_state.torrents.insert(hidden_hash.clone(), hidden);

        app_state.torrent_list_order = vec![visible_hash];
        app_state.activity_history_state.torrents.insert(
            hidden_key.clone(),
            ActivityHistorySeries {
                tiers: ActivityHistoryTiers {
                    second_1s: vec![ActivityHistoryPoint {
                        ts_unix: 1,
                        primary: 1,
                        secondary: 2,
                    }],
                    ..Default::default()
                },
                ..Default::default()
            },
        );

        ActivityHistoryTelemetry::on_second_tick(&mut app_state);

        let hidden_series = app_state
            .activity_history_state
            .torrents
            .get(&hidden_key)
            .expect("hidden torrent history should be preserved");
        assert_eq!(hidden_series.tiers.second_1s.len(), 2);

        let latest_point = hidden_series
            .tiers
            .second_1s
            .last()
            .expect("latest point should exist");
        assert_eq!(latest_point.primary, 20);
        assert_eq!(latest_point.secondary, 8);
    }
}
