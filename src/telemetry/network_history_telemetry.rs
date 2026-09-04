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
const AUTO_GRAPH_FIXED_MODES: [GraphDisplayMode; 11] = [
    GraphDisplayMode::OneMinute,
    GraphDisplayMode::FiveMinutes,
    GraphDisplayMode::TenMinutes,
    GraphDisplayMode::ThirtyMinutes,
    GraphDisplayMode::OneHour,
    GraphDisplayMode::ThreeHours,
    GraphDisplayMode::TwelveHours,
    GraphDisplayMode::TwentyFourHours,
    GraphDisplayMode::SevenDays,
    GraphDisplayMode::ThirtyDays,
    GraphDisplayMode::OneYear,
];

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

fn graph_mode_points(
    state: &NetworkHistoryPersistedState,
    mode: GraphDisplayMode,
) -> (&[NetworkHistoryPoint], u64) {
    match mode {
        GraphDisplayMode::Auto
        | GraphDisplayMode::OneMinute
        | GraphDisplayMode::FiveMinutes
        | GraphDisplayMode::TenMinutes
        | GraphDisplayMode::ThirtyMinutes
        | GraphDisplayMode::OneHour => (&state.tiers.second_1s, 1),
        GraphDisplayMode::ThreeHours
        | GraphDisplayMode::TwelveHours
        | GraphDisplayMode::TwentyFourHours => (&state.tiers.minute_1m, 60),
        GraphDisplayMode::SevenDays | GraphDisplayMode::ThirtyDays => {
            (&state.tiers.minute_15m, 15 * 60)
        }
        GraphDisplayMode::OneYear => (&state.tiers.hour_1h, 60 * 60),
    }
}

fn graph_mode_rank(mode: GraphDisplayMode) -> usize {
    AUTO_GRAPH_FIXED_MODES
        .iter()
        .position(|candidate| *candidate == mode)
        .unwrap_or(0)
}

fn auto_graph_required_history_secs(mode: GraphDisplayMode) -> u64 {
    match mode {
        GraphDisplayMode::Auto | GraphDisplayMode::OneMinute => 0,
        GraphDisplayMode::FiveMinutes => GraphDisplayMode::OneMinute.as_seconds() as u64,
        GraphDisplayMode::TenMinutes => GraphDisplayMode::FiveMinutes.as_seconds() as u64,
        GraphDisplayMode::ThirtyMinutes => GraphDisplayMode::TenMinutes.as_seconds() as u64,
        GraphDisplayMode::OneHour => GraphDisplayMode::ThirtyMinutes.as_seconds() as u64,
        GraphDisplayMode::ThreeHours => GraphDisplayMode::OneHour.as_seconds() as u64,
        GraphDisplayMode::TwelveHours => GraphDisplayMode::ThreeHours.as_seconds() as u64,
        GraphDisplayMode::TwentyFourHours => GraphDisplayMode::TwelveHours.as_seconds() as u64,
        GraphDisplayMode::SevenDays => GraphDisplayMode::TwentyFourHours.as_seconds() as u64,
        GraphDisplayMode::ThirtyDays => GraphDisplayMode::SevenDays.as_seconds() as u64,
        GraphDisplayMode::OneYear => GraphDisplayMode::ThirtyDays.as_seconds() as u64,
    }
}

fn step_graph_mode_toward(current: GraphDisplayMode, target: GraphDisplayMode) -> GraphDisplayMode {
    let current_rank = graph_mode_rank(current);
    let target_rank = graph_mode_rank(target);
    if target_rank > current_rank {
        AUTO_GRAPH_FIXED_MODES[current_rank + 1]
    } else if target_rank < current_rank {
        AUTO_GRAPH_FIXED_MODES[current_rank - 1]
    } else {
        current
    }
}

fn auto_graph_candidate_score(
    state: &NetworkHistoryPersistedState,
    mode: GraphDisplayMode,
    now_unix: u64,
) -> Option<f64> {
    let window_secs = mode.as_seconds() as u64;
    let window_start = now_unix.saturating_sub(window_secs);
    let (tier, step_secs) = graph_mode_points(state, mode);
    let oldest_ts = tier.first()?.ts_unix;
    let available_span = now_unix.saturating_sub(oldest_ts).saturating_add(step_secs);
    if available_span < auto_graph_required_history_secs(mode) {
        return None;
    }

    let points = tier
        .iter()
        .filter(|point| point.ts_unix >= window_start && point.ts_unix <= now_unix)
        .collect::<Vec<_>>();
    if points.is_empty() {
        return (mode == GraphDisplayMode::OneMinute).then_some(0.0);
    }

    let peak = points
        .iter()
        .map(|point| point.download_bps.saturating_add(point.upload_bps))
        .max()
        .unwrap_or_default();
    if peak == 0 {
        return (mode == GraphDisplayMode::OneMinute).then_some(0.0);
    }
    let activity_threshold = (peak / 50).max(1_024);
    let active = points
        .iter()
        .filter_map(|point| {
            let rate = point.download_bps.saturating_add(point.upload_bps);
            (rate >= activity_threshold).then_some((point.ts_unix, rate))
        })
        .collect::<Vec<_>>();
    let (first_active, last_active) = (active.first()?, active.last()?);
    let first_position = first_active.0.saturating_sub(window_start) as f64 / window_secs as f64;
    let last_position = last_active.0.saturating_sub(window_start) as f64 / window_secs as f64;
    let activity_span = (last_position - first_position).clamp(0.0, 1.0);
    let story_fit = (1.0 - (activity_span - 0.65).abs() / 0.65).clamp(0.0, 1.0);
    let lead_in_fit = (1.0 - (first_position - 0.15).abs() / 0.35).clamp(0.0, 1.0);
    let minimum = active.iter().map(|(_, rate)| *rate).min().unwrap_or(peak);
    let dynamic_range = peak.saturating_sub(minimum) as f64 / peak as f64;
    let transition_count = points
        .windows(2)
        .filter(|pair| {
            let left =
                pair[0].download_bps.saturating_add(pair[0].upload_bps) >= activity_threshold;
            let right =
                pair[1].download_bps.saturating_add(pair[1].upload_bps) >= activity_threshold;
            left != right
        })
        .count();
    let transition_score = (transition_count as f64 / 4.0).min(1.0);
    let recent_score = if now_unix.saturating_sub(last_active.0) <= step_secs * 2 {
        1.0
    } else {
        0.0
    };
    let clips_start = first_active.0 <= window_start.saturating_add(window_secs / 20);

    Some(
        0.35 * story_fit
            + 0.25 * lead_in_fit
            + 0.20 * dynamic_range
            + 0.10 * transition_score
            + 0.10 * recent_score
            - if clips_start { 0.25 } else { 0.0 },
    )
}

fn recommended_auto_graph_mode(app_state: &AppState, now_unix: u64) -> GraphDisplayMode {
    AUTO_GRAPH_FIXED_MODES
        .iter()
        .copied()
        .filter_map(|mode| {
            auto_graph_candidate_score(&app_state.network_history_state, mode, now_unix)
                .map(|score| (mode, score))
        })
        .max_by(|left, right| {
            left.1
                .total_cmp(&right.1)
                .then_with(|| graph_mode_rank(left.0).cmp(&graph_mode_rank(right.0)))
        })
        .map(|(mode, _)| mode)
        .unwrap_or(GraphDisplayMode::OneMinute)
}

fn update_auto_graph_window(app_state: &mut AppState, now_unix: u64) {
    if app_state.graph_mode != GraphDisplayMode::Auto
        || (app_state.auto_graph_window.last_evaluation_unix != 0
            && now_unix.saturating_sub(app_state.auto_graph_window.last_evaluation_unix)
                < AUTO_GRAPH_EVALUATION_INTERVAL_SECS)
    {
        return;
    }
    app_state.auto_graph_window.last_evaluation_unix = now_unix;
    let target = recommended_auto_graph_mode(app_state, now_unix);
    let current = app_state.auto_graph_window.effective_mode;
    if target == current {
        return;
    }

    let zooming_in = graph_mode_rank(target) < graph_mode_rank(current);
    let dwell_complete = app_state.auto_graph_window.last_change_unix == 0
        || now_unix.saturating_sub(app_state.auto_graph_window.last_change_unix)
            >= AUTO_GRAPH_MINIMUM_DWELL_SECS;
    if zooming_in || dwell_complete {
        app_state.auto_graph_window.effective_mode = step_graph_mode_toward(current, target);
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
        recommended_auto_graph_mode, update_auto_graph_window, NetworkHistoryTelemetry,
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

    #[test]
    fn auto_graph_starts_at_one_minute_without_enough_history() {
        let mut app_state = AppState {
            graph_mode: GraphDisplayMode::Auto,
            ..Default::default()
        };
        app_state.network_history_state.tiers.second_1s = (1..=30)
            .map(|second| point(second, 10_000, 1_000))
            .collect();

        assert_eq!(
            recommended_auto_graph_mode(&app_state, 30),
            GraphDisplayMode::OneMinute
        );
    }

    #[test]
    fn auto_graph_widens_to_frame_a_sustained_transfer() {
        let mut app_state = AppState {
            graph_mode: GraphDisplayMode::Auto,
            ..Default::default()
        };
        app_state.network_history_state.tiers.second_1s = (941..=1_000)
            .map(|second| point(second, 10_000, 0))
            .collect();

        assert_eq!(
            recommended_auto_graph_mode(&app_state, 1_000),
            GraphDisplayMode::FiveMinutes
        );
    }

    #[test]
    fn auto_graph_zooms_in_for_a_new_activity_burst() {
        let mut app_state = AppState {
            graph_mode: GraphDisplayMode::Auto,
            ..Default::default()
        };
        app_state.network_history_state.tiers.second_1s = (1..=600)
            .map(|second| point(second, u64::from(second >= 581) * 25_000, 0))
            .collect();

        assert_eq!(
            recommended_auto_graph_mode(&app_state, 600),
            GraphDisplayMode::OneMinute
        );
    }

    #[test]
    fn auto_graph_widens_one_step_per_dwell_period() {
        let mut app_state = AppState {
            graph_mode: GraphDisplayMode::Auto,
            ..Default::default()
        };
        app_state.network_history_state.tiers.second_1s = (1..=600)
            .map(|second| point(second, u64::from(second >= 121) * 10_000, 0))
            .collect();

        update_auto_graph_window(&mut app_state, 600);
        assert_eq!(
            app_state.auto_graph_window.effective_mode,
            GraphDisplayMode::FiveMinutes
        );
        update_auto_graph_window(&mut app_state, 605);
        assert_eq!(
            app_state.auto_graph_window.effective_mode,
            GraphDisplayMode::FiveMinutes
        );
        update_auto_graph_window(&mut app_state, 620);
        assert_eq!(
            app_state.auto_graph_window.effective_mode,
            GraphDisplayMode::TenMinutes
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
