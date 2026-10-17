// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

pub(crate) fn densify_points_for_restore<T, FTs, FZero>(
    points: &[T],
    step_secs: u64,
    max_points: usize,
    now_unix: u64,
    point_ts: FTs,
    zero_point_at: FZero,
) -> Vec<T>
where
    T: Clone,
    FTs: Fn(&T) -> u64,
    FZero: Fn(u64) -> T,
{
    if points.is_empty() || step_secs == 0 || max_points == 0 {
        return Vec::new();
    }

    let dense_end_ts = densified_end_ts(point_ts(&points[points.len() - 1]), step_secs, now_unix);
    let max_window_span = step_secs.saturating_mul(max_points.saturating_sub(1) as u64);
    let dense_start_ts = point_ts(&points[0]).max(dense_end_ts.saturating_sub(max_window_span));

    let mut start_idx = 0;
    while start_idx < points.len() && point_ts(&points[start_idx]) < dense_start_ts {
        start_idx += 1;
    }

    // Allocate only as samples are produced; short histories need no full-window reservation.
    let mut dense = Vec::new();
    let mut next_ts = dense_start_ts;

    for point in &points[start_idx..] {
        while next_ts < point_ts(point) && next_ts <= dense_end_ts {
            push_restore_point(&mut dense, zero_point_at(next_ts), max_points);
            let advanced_ts = next_ts.saturating_add(step_secs);
            if advanced_ts == next_ts {
                return dense;
            }
            next_ts = advanced_ts;
        }

        if next_ts > dense_end_ts {
            break;
        }

        push_restore_point(&mut dense, point.clone(), max_points);
        if point_ts(point) >= dense_end_ts {
            return dense;
        }

        let advanced_ts = point_ts(point).saturating_add(step_secs);
        if advanced_ts == point_ts(point) {
            return dense;
        }
        next_ts = advanced_ts;
    }

    while next_ts <= dense_end_ts {
        push_restore_point(&mut dense, zero_point_at(next_ts), max_points);
        let advanced_ts = next_ts.saturating_add(step_secs);
        if advanced_ts == next_ts {
            break;
        }
        next_ts = advanced_ts;
    }

    dense
}

fn push_restore_point<T>(points: &mut Vec<T>, point: T, max_points: usize) {
    if points.len() == points.capacity() {
        // Bound spare capacity even when restoring a nearly full window. Retain
        // legacy output for irregular timestamps that produce more than the cap.
        let additional = points
            .len()
            .clamp(4, 64)
            .min(max_points.saturating_sub(points.len()).max(1));
        points.reserve_exact(additional);
    }
    points.push(point);
}

fn densified_end_ts(last_point_ts: u64, step_secs: u64, now_unix: u64) -> u64 {
    if last_point_ts >= now_unix {
        return last_point_ts;
    }

    let trailing_steps = (now_unix - last_point_ts) / step_secs;
    last_point_ts.saturating_add(trailing_steps.saturating_mul(step_secs))
}

#[cfg(test)]
mod tests {
    use super::{densified_end_ts, densify_points_for_restore};

    // Frozen restore implementation from the parent revision.
    fn legacy_densify_points_for_restore<T, FTs, FZero>(
        points: &[T],
        step_secs: u64,
        max_points: usize,
        now_unix: u64,
        point_ts: FTs,
        zero_point_at: FZero,
    ) -> Vec<T>
    where
        T: Clone,
        FTs: Fn(&T) -> u64,
        FZero: Fn(u64) -> T,
    {
        if points.is_empty() || step_secs == 0 || max_points == 0 {
            return Vec::new();
        }

        let dense_end_ts =
            densified_end_ts(point_ts(&points[points.len() - 1]), step_secs, now_unix);
        let max_window_span = step_secs.saturating_mul(max_points.saturating_sub(1) as u64);
        let dense_start_ts = point_ts(&points[0]).max(dense_end_ts.saturating_sub(max_window_span));

        let mut start_idx = 0;
        while start_idx < points.len() && point_ts(&points[start_idx]) < dense_start_ts {
            start_idx += 1;
        }

        let mut dense = Vec::with_capacity(max_points);
        let mut next_ts = dense_start_ts;

        for point in &points[start_idx..] {
            while next_ts < point_ts(point) && next_ts <= dense_end_ts {
                dense.push(zero_point_at(next_ts));
                let advanced_ts = next_ts.saturating_add(step_secs);
                if advanced_ts == next_ts {
                    return dense;
                }
                next_ts = advanced_ts;
            }

            if next_ts > dense_end_ts {
                break;
            }

            dense.push(point.clone());
            if point_ts(point) >= dense_end_ts {
                return dense;
            }

            let advanced_ts = point_ts(point).saturating_add(step_secs);
            if advanced_ts == point_ts(point) {
                return dense;
            }
            next_ts = advanced_ts;
        }

        while next_ts <= dense_end_ts {
            dense.push(zero_point_at(next_ts));
            let advanced_ts = next_ts.saturating_add(step_secs);
            if advanced_ts == next_ts {
                break;
            }
            next_ts = advanced_ts;
        }

        dense
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct TestPoint {
        ts_unix: u64,
        value: u64,
    }

    #[test]
    fn densify_points_for_restore_fills_sparse_gaps_and_tail() {
        let dense = densify_points_for_restore(
            &[
                TestPoint {
                    ts_unix: 1,
                    value: 10,
                },
                TestPoint {
                    ts_unix: 3,
                    value: 30,
                },
            ],
            1,
            8,
            4,
            |point| point.ts_unix,
            |ts_unix| TestPoint { ts_unix, value: 0 },
        );

        assert_eq!(
            dense.iter().map(|point| point.value).collect::<Vec<_>>(),
            vec![10, 0, 30, 0]
        );
    }

    #[test]
    fn densify_points_for_restore_limits_fill_to_retention_window() {
        let dense = densify_points_for_restore(
            &[TestPoint {
                ts_unix: 1,
                value: 10,
            }],
            1,
            4,
            1_000_000,
            |point| point.ts_unix,
            |ts_unix| TestPoint { ts_unix, value: 0 },
        );

        assert_eq!(
            dense.iter().map(|point| point.ts_unix).collect::<Vec<_>>(),
            vec![999_997, 999_998, 999_999, 1_000_000]
        );
        assert!(dense.iter().all(|point| point.value == 0));
    }

    #[test]
    fn restore_short_history_does_not_reserve_full_retention() {
        let points = [TestPoint {
            ts_unix: 100,
            value: 7,
        }];
        let dense = densify_points_for_restore(
            &points,
            1,
            8_760,
            102,
            |point| point.ts_unix,
            |ts_unix| TestPoint { ts_unix, value: 0 },
        );
        assert_eq!(
            dense,
            vec![
                points[0].clone(),
                TestPoint {
                    ts_unix: 101,
                    value: 0
                },
                TestPoint {
                    ts_unix: 102,
                    value: 0
                }
            ]
        );
        assert!(dense.capacity() < 64);
        let full = densify_points_for_restore(
            &points,
            1,
            8_760,
            8_859,
            |point| point.ts_unix,
            |ts_unix| TestPoint { ts_unix, value: 0 },
        );
        assert_eq!(full.len(), 8_760);
        assert_eq!(full.capacity(), 8_760);
        let empty = densify_points_for_restore::<TestPoint, _, _>(
            &[],
            1,
            8_760,
            102,
            |point| point.ts_unix,
            |ts_unix| TestPoint { ts_unix, value: 0 },
        );
        assert_eq!(empty.capacity(), 0);
    }

    #[test]
    fn restore_matches_legacy_for_retention_irregular_and_saturated_timestamps() {
        let cases: &[&[u64]] = &[
            &[],
            &[0],
            &[1],
            &[1, 3, 8],
            &[8, 8, 8],
            &[1, 2, 2, 3],
            &[9, 3, 7],
            &[u64::MAX - 7, u64::MAX - 3, u64::MAX],
            &[u64::MAX],
        ];
        for timestamps in cases {
            let points: Vec<_> = timestamps
                .iter()
                .enumerate()
                .map(|(i, &ts_unix)| TestPoint {
                    ts_unix,
                    value: i as u64 + 1,
                })
                .collect();
            for step in [0, 1, 2, 7, u64::MAX] {
                for cap in [0, 1, 2, 7, 64, 65] {
                    for now in [0, 3, 10, 1_000, u64::MAX - 1, u64::MAX] {
                        let actual = densify_points_for_restore(
                            &points,
                            step,
                            cap,
                            now,
                            |point| point.ts_unix,
                            |ts_unix| TestPoint { ts_unix, value: 0 },
                        );
                        let expected = legacy_densify_points_for_restore(
                            &points,
                            step,
                            cap,
                            now,
                            |point| point.ts_unix,
                            |ts_unix| TestPoint { ts_unix, value: 0 },
                        );
                        assert_eq!(
                            actual, expected,
                            "timestamps={timestamps:?}, step={step}, cap={cap}, now={now}"
                        );
                    }
                }
            }
        }
    }
}
