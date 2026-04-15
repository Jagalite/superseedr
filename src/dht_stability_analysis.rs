// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later
// TEMP-BENCHMARK-ONLY: remove this temporary analyzer before pushing.

use chrono::{DateTime, Duration, FixedOffset, Timelike};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct DhtStabilityAnalysisOptions {
    pub window_minutes: u32,
}

#[derive(Debug, Clone)]
pub struct DhtStabilityAnalysis {
    pub text: String,
    pub data: Value,
}

#[derive(Debug, Clone, Default)]
struct FamilyWindowAccumulator {
    count: usize,
    active_routes_sum: f64,
    query_success_sum: f64,
    peers_sum: f64,
    first_batch_unique_sum: f64,
    first_value_bootstrap: usize,
    first_value_seed: usize,
    first_value_discovered: usize,
    first_value_none: usize,
    frontier_exhausted: usize,
    peer_limit_reached: usize,
    visit_cap_reached: usize,
}

#[derive(Debug, Clone)]
struct FamilyWindowSummary {
    window_start: DateTime<FixedOffset>,
    count: usize,
    active_routes_avg: f64,
    query_success_avg: f64,
    peers_avg: f64,
    first_batch_unique_avg: f64,
    first_value_bootstrap: usize,
    first_value_seed: usize,
    first_value_discovered: usize,
    first_value_none: usize,
    frontier_exhausted: usize,
    peer_limit_reached: usize,
    visit_cap_reached: usize,
    status: &'static str,
}

#[derive(Debug, Clone, Default)]
struct HealthSnapshotSummary {
    count: usize,
    recovery_pending_count: usize,
    warning_count: usize,
    cached_ipv4_routes_min: Option<usize>,
    cached_ipv4_routes_max: Option<usize>,
    cached_ipv4_routes_last: Option<usize>,
    cached_ipv6_routes_min: Option<usize>,
    cached_ipv6_routes_max: Option<usize>,
    cached_ipv6_routes_last: Option<usize>,
}

pub fn analyze_dht_stability(
    path: &Path,
    options: &DhtStabilityAnalysisOptions,
) -> Result<DhtStabilityAnalysis, String> {
    let window_minutes = options.window_minutes.max(1);
    let file = File::open(path).map_err(|error| {
        format!(
            "Failed to open metrics file '{}': {}",
            path.display(),
            error
        )
    })?;
    let reader = BufReader::new(file);

    let mut total_events = 0usize;
    let mut session_ids = BTreeSet::new();
    let mut first_ts: Option<DateTime<FixedOffset>> = None;
    let mut last_ts: Option<DateTime<FixedOffset>> = None;
    let mut lookup_windows: HashMap<
        String,
        BTreeMap<DateTime<FixedOffset>, FamilyWindowAccumulator>,
    > = HashMap::new();
    let mut health = HealthSnapshotSummary::default();

    for (line_index, line) in reader.lines().enumerate() {
        let line = line.map_err(|error| {
            format!(
                "Failed reading metrics file '{}' on line {}: {}",
                path.display(),
                line_index + 1,
                error
            )
        })?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let event: Value = serde_json::from_str(trimmed).map_err(|error| {
            format!(
                "Malformed JSON in metrics file '{}' on line {}: {}",
                path.display(),
                line_index + 1,
                error
            )
        })?;
        let event_object = event
            .as_object()
            .ok_or_else(|| format!("Expected JSON object on line {}", line_index + 1))?;
        let ts = parse_event_ts(event_object.get("ts"))?
            .ok_or_else(|| format!("Missing timestamp on line {}", line_index + 1))?;
        let event_type = event_object
            .get("event_type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let fields = event_object
            .get("fields")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        total_events += 1;
        if let Some(session_id) = event_object.get("session_id").and_then(Value::as_str) {
            session_ids.insert(session_id.to_string());
        }
        first_ts = Some(first_ts.map_or(ts, |current| current.min(ts)));
        last_ts = Some(last_ts.map_or(ts, |current| current.max(ts)));

        match event_type {
            "dht_internal_family_summary" => {
                let purpose =
                    field_as_string(&fields, "purpose").unwrap_or_else(|| "unknown".into());
                if purpose != "lookup" {
                    continue;
                }
                let family = field_as_string(&fields, "family").unwrap_or_else(|| "unknown".into());
                let window_start = floor_to_window(ts, window_minutes);
                let family_windows = lookup_windows.entry(family).or_default();
                let entry = family_windows.entry(window_start).or_default();
                entry.count += 1;
                entry.active_routes_sum +=
                    field_as_f64(&fields, "active_routes_available").unwrap_or(0.0);
                entry.query_success_sum += field_as_f64(&fields, "query_successes").unwrap_or(0.0);
                entry.peers_sum += field_as_f64(&fields, "peers").unwrap_or(0.0);
                entry.first_batch_unique_sum +=
                    field_as_f64(&fields, "unique_peers_before_first_batch").unwrap_or(0.0);
                match field_as_string(&fields, "first_value_source").as_deref() {
                    Some("bootstrap") => entry.first_value_bootstrap += 1,
                    Some("seed") => entry.first_value_seed += 1,
                    Some("discovered") => entry.first_value_discovered += 1,
                    _ => entry.first_value_none += 1,
                }
                match field_as_string(&fields, "ended_reason").as_deref() {
                    Some("frontier_exhausted") => entry.frontier_exhausted += 1,
                    Some("peer_limit_reached") => entry.peer_limit_reached += 1,
                    Some("visit_cap_reached") => entry.visit_cap_reached += 1,
                    _ => {}
                }
            }
            "dht_health_snapshot" => {
                health.count += 1;
                if fields
                    .get("recovery_pending")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    health.recovery_pending_count += 1;
                }
                if !fields.get("warning").map(Value::is_null).unwrap_or(true) {
                    health.warning_count += 1;
                }
                update_route_extrema(
                    &mut health.cached_ipv4_routes_min,
                    &mut health.cached_ipv4_routes_max,
                    &mut health.cached_ipv4_routes_last,
                    field_as_usize(&fields, "cached_ipv4_routes"),
                );
                update_route_extrema(
                    &mut health.cached_ipv6_routes_min,
                    &mut health.cached_ipv6_routes_max,
                    &mut health.cached_ipv6_routes_last,
                    field_as_usize(&fields, "cached_ipv6_routes"),
                );
            }
            _ => {}
        }
    }

    let family_reports = lookup_windows
        .into_iter()
        .map(|(family, windows)| build_family_report(&family, windows))
        .collect::<Vec<_>>();

    let data = json!({
        "events": total_events,
        "window_minutes": window_minutes,
        "session_summary": {
            "unique_sessions": session_ids.len(),
            "session_id": (session_ids.len() == 1).then(|| session_ids.iter().next().cloned()).flatten(),
            "start": first_ts.map(|ts| ts.to_rfc3339()),
            "end": last_ts.map(|ts| ts.to_rfc3339()),
            "duration_seconds": match (first_ts, last_ts) {
                (Some(start), Some(end)) => Some((end - start).num_milliseconds() as f64 / 1000.0),
                _ => None,
            },
        },
        "health_snapshots": {
            "count": health.count,
            "recovery_pending_count": health.recovery_pending_count,
            "warning_count": health.warning_count,
            "cached_ipv4_routes": {
                "min": health.cached_ipv4_routes_min,
                "max": health.cached_ipv4_routes_max,
                "last": health.cached_ipv4_routes_last,
            },
            "cached_ipv6_routes": {
                "min": health.cached_ipv6_routes_min,
                "max": health.cached_ipv6_routes_max,
                "last": health.cached_ipv6_routes_last,
            }
        },
        "families": family_reports,
    });
    let text = render_text(&data);

    Ok(DhtStabilityAnalysis { text, data })
}

fn build_family_report(
    family: &str,
    windows: BTreeMap<DateTime<FixedOffset>, FamilyWindowAccumulator>,
) -> Value {
    let active_baseline = p90_nonzero(
        &windows
            .values()
            .filter_map(|item| {
                (item.count > 0).then_some(item.active_routes_sum / item.count as f64)
            })
            .collect::<Vec<_>>(),
    );
    let success_baseline = p90_nonzero(
        &windows
            .values()
            .filter_map(|item| {
                (item.count > 0).then_some(item.query_success_sum / item.count as f64)
            })
            .collect::<Vec<_>>(),
    );
    let peers_baseline = p90_nonzero(
        &windows
            .values()
            .filter_map(|item| (item.count > 0).then_some(item.peers_sum / item.count as f64))
            .collect::<Vec<_>>(),
    );
    let first_batch_baseline = p90_nonzero(
        &windows
            .values()
            .filter_map(|item| {
                (item.count > 0).then_some(item.first_batch_unique_sum / item.count as f64)
            })
            .collect::<Vec<_>>(),
    );

    let window_summaries = windows
        .into_iter()
        .map(|(window_start, item)| {
            let count = item.count.max(1);
            let active_routes_avg = item.active_routes_sum / count as f64;
            let query_success_avg = item.query_success_sum / count as f64;
            let peers_avg = item.peers_sum / count as f64;
            let first_batch_unique_avg = item.first_batch_unique_sum / count as f64;
            let status = classify_window(
                active_routes_avg,
                query_success_avg,
                peers_avg,
                first_batch_unique_avg,
                item.frontier_exhausted,
                count,
                active_baseline,
                success_baseline,
                peers_baseline,
                first_batch_baseline,
            );
            FamilyWindowSummary {
                window_start,
                count: item.count,
                active_routes_avg,
                query_success_avg,
                peers_avg,
                first_batch_unique_avg,
                first_value_bootstrap: item.first_value_bootstrap,
                first_value_seed: item.first_value_seed,
                first_value_discovered: item.first_value_discovered,
                first_value_none: item.first_value_none,
                frontier_exhausted: item.frontier_exhausted,
                peer_limit_reached: item.peer_limit_reached,
                visit_cap_reached: item.visit_cap_reached,
                status,
            }
        })
        .collect::<Vec<_>>();

    let healthy_windows = window_summaries
        .iter()
        .filter(|item| item.status == "healthy")
        .count();
    let degraded_windows = window_summaries
        .iter()
        .filter(|item| item.status == "degraded")
        .count();
    let collapsed_windows = window_summaries
        .iter()
        .filter(|item| item.status == "collapsed")
        .count();
    let collapse_episodes = count_episodes(&window_summaries, "collapsed");
    let degraded_episodes = count_episodes(&window_summaries, "degraded");
    let first_collapsed_window = window_summaries
        .iter()
        .find(|item| item.status == "collapsed")
        .map(|item| item.window_start.to_rfc3339());
    let last_window_status = window_summaries.last().map(|item| item.status.to_string());
    let notable_windows = window_summaries
        .iter()
        .filter(|item| item.status != "healthy")
        .map(|item| {
            json!({
                "window_start": item.window_start.to_rfc3339(),
                "status": item.status,
                "count": item.count,
                "active_routes_avg": item.active_routes_avg,
                "query_success_avg": item.query_success_avg,
                "peers_avg": item.peers_avg,
                "first_batch_unique_avg": item.first_batch_unique_avg,
                "first_value_sources": {
                    "bootstrap": item.first_value_bootstrap,
                    "seed": item.first_value_seed,
                    "discovered": item.first_value_discovered,
                    "none": item.first_value_none,
                },
                "frontier_exhausted": item.frontier_exhausted,
                "peer_limit_reached": item.peer_limit_reached,
                "visit_cap_reached": item.visit_cap_reached,
            })
        })
        .collect::<Vec<_>>();

    json!({
        "family": family,
        "baseline": {
            "active_routes_avg_p90": active_baseline,
            "query_success_avg_p90": success_baseline,
            "peers_avg_p90": peers_baseline,
            "first_batch_unique_avg_p90": first_batch_baseline,
        },
        "summary": {
            "window_count": window_summaries.len(),
            "healthy_windows": healthy_windows,
            "degraded_windows": degraded_windows,
            "collapsed_windows": collapsed_windows,
            "degraded_episodes": degraded_episodes,
            "collapse_episodes": collapse_episodes,
            "first_collapsed_window": first_collapsed_window,
            "last_window_status": last_window_status,
        },
        "windows": window_summaries.iter().map(|item| {
            json!({
                "window_start": item.window_start.to_rfc3339(),
                "status": item.status,
                "count": item.count,
                "active_routes_avg": item.active_routes_avg,
                "query_success_avg": item.query_success_avg,
                "peers_avg": item.peers_avg,
                "first_batch_unique_avg": item.first_batch_unique_avg,
                "first_value_sources": {
                    "bootstrap": item.first_value_bootstrap,
                    "seed": item.first_value_seed,
                    "discovered": item.first_value_discovered,
                    "none": item.first_value_none,
                },
                "frontier_exhausted": item.frontier_exhausted,
                "peer_limit_reached": item.peer_limit_reached,
                "visit_cap_reached": item.visit_cap_reached,
            })
        }).collect::<Vec<_>>(),
        "notable_windows": notable_windows,
    })
}

fn classify_window(
    active_routes_avg: f64,
    query_success_avg: f64,
    peers_avg: f64,
    first_batch_unique_avg: f64,
    frontier_exhausted: usize,
    count: usize,
    active_baseline: f64,
    success_baseline: f64,
    peers_baseline: f64,
    first_batch_baseline: f64,
) -> &'static str {
    let frontier_ratio = frontier_exhausted as f64 / count.max(1) as f64;
    let active_ratio = relative_ratio(active_routes_avg, active_baseline);
    let success_ratio = relative_ratio(query_success_avg, success_baseline);
    let peers_ratio = relative_ratio(peers_avg, peers_baseline);
    let first_batch_ratio = relative_ratio(first_batch_unique_avg, first_batch_baseline);

    if (peers_avg == 0.0 && query_success_avg == 0.0)
        || (frontier_ratio >= 0.90
            && peers_ratio <= 0.10
            && success_ratio <= 0.10
            && active_ratio <= 0.25)
    {
        return "collapsed";
    }

    if frontier_ratio >= 0.50
        || peers_ratio <= 0.60
        || success_ratio <= 0.60
        || active_ratio <= 0.60
        || first_batch_ratio <= 0.50
    {
        return "degraded";
    }

    "healthy"
}

fn render_text(data: &Value) -> String {
    let mut lines = Vec::new();
    lines.push("DHT Stability Report".to_string());
    lines.push(format!(
        "events: {}",
        value_usize(data, "events").unwrap_or(0)
    ));
    lines.push(format!(
        "window_minutes: {}",
        value_usize(data, "window_minutes").unwrap_or(1)
    ));
    lines.push(String::new());

    if let Some(session) = data.get("session_summary") {
        lines.push("Session summary:".to_string());
        lines.push(format!(
            "  unique_sessions: {}",
            value_usize(session, "unique_sessions").unwrap_or(0)
        ));
        if let Some(session_id) = session.get("session_id").and_then(Value::as_str) {
            lines.push(format!("  session_id: {}", session_id));
        }
        if let Some(start) = session.get("start").and_then(Value::as_str) {
            lines.push(format!("  start: {}", start));
        }
        if let Some(end) = session.get("end").and_then(Value::as_str) {
            lines.push(format!("  end: {}", end));
        }
        if let Some(duration) = session.get("duration_seconds").and_then(Value::as_f64) {
            lines.push(format!("  duration_seconds: {:.3}", duration));
        }
        lines.push(String::new());
    }

    if let Some(health) = data.get("health_snapshots") {
        lines.push("Health snapshots:".to_string());
        lines.push(format!(
            "  count={} recovery_pending_count={} warning_count={}",
            value_usize(health, "count").unwrap_or(0),
            value_usize(health, "recovery_pending_count").unwrap_or(0),
            value_usize(health, "warning_count").unwrap_or(0),
        ));
        if let Some(cached_ipv4) = health.get("cached_ipv4_routes") {
            lines.push(format!(
                "  cached_ipv4_routes: min={} max={} last={}",
                format_optional_usize(cached_ipv4.get("min").and_then(Value::as_u64)),
                format_optional_usize(cached_ipv4.get("max").and_then(Value::as_u64)),
                format_optional_usize(cached_ipv4.get("last").and_then(Value::as_u64)),
            ));
        }
        if let Some(cached_ipv6) = health.get("cached_ipv6_routes") {
            lines.push(format!(
                "  cached_ipv6_routes: min={} max={} last={}",
                format_optional_usize(cached_ipv6.get("min").and_then(Value::as_u64)),
                format_optional_usize(cached_ipv6.get("max").and_then(Value::as_u64)),
                format_optional_usize(cached_ipv6.get("last").and_then(Value::as_u64)),
            ));
        }
        lines.push(String::new());
    }

    if let Some(families) = data.get("families").and_then(Value::as_array) {
        lines.push("Families:".to_string());
        for family in families {
            let summary = family.get("summary").cloned().unwrap_or(Value::Null);
            let baseline = family.get("baseline").cloned().unwrap_or(Value::Null);
            lines.push(format!(
                "  {}: windows={} healthy={} degraded={} collapsed={} degraded_episodes={} collapse_episodes={} last_window_status={}",
                family.get("family").and_then(Value::as_str).unwrap_or("unknown"),
                value_usize(&summary, "window_count").unwrap_or(0),
                value_usize(&summary, "healthy_windows").unwrap_or(0),
                value_usize(&summary, "degraded_windows").unwrap_or(0),
                value_usize(&summary, "collapsed_windows").unwrap_or(0),
                value_usize(&summary, "degraded_episodes").unwrap_or(0),
                value_usize(&summary, "collapse_episodes").unwrap_or(0),
                summary.get("last_window_status").and_then(Value::as_str).unwrap_or("unknown"),
            ));
            lines.push(format!(
                "    baseline: active_routes_p90={} query_success_p90={} peers_p90={} first_batch_unique_p90={}",
                format_optional_float(baseline.get("active_routes_avg_p90").and_then(Value::as_f64)),
                format_optional_float(baseline.get("query_success_avg_p90").and_then(Value::as_f64)),
                format_optional_float(baseline.get("peers_avg_p90").and_then(Value::as_f64)),
                format_optional_float(baseline.get("first_batch_unique_avg_p90").and_then(Value::as_f64)),
            ));
            if let Some(first_collapsed) = summary
                .get("first_collapsed_window")
                .and_then(Value::as_str)
            {
                lines.push(format!("    first_collapsed_window: {}", first_collapsed));
            }
            if let Some(notable_windows) = family.get("notable_windows").and_then(Value::as_array) {
                for window in notable_windows.iter().take(8) {
                    lines.push(format!(
                        "    {} {} active_routes_avg={} query_success_avg={} peers_avg={} first_batch_unique_avg={} first_value_sources=bootstrap:{} seed:{} discovered:{} none:{} frontier_exhausted={}",
                        window.get("window_start").and_then(Value::as_str).unwrap_or("unknown"),
                        window.get("status").and_then(Value::as_str).unwrap_or("unknown"),
                        format_optional_float(window.get("active_routes_avg").and_then(Value::as_f64)),
                        format_optional_float(window.get("query_success_avg").and_then(Value::as_f64)),
                        format_optional_float(window.get("peers_avg").and_then(Value::as_f64)),
                        format_optional_float(window.get("first_batch_unique_avg").and_then(Value::as_f64)),
                        value_path_usize(window, &["first_value_sources", "bootstrap"]).unwrap_or(0),
                        value_path_usize(window, &["first_value_sources", "seed"]).unwrap_or(0),
                        value_path_usize(window, &["first_value_sources", "discovered"]).unwrap_or(0),
                        value_path_usize(window, &["first_value_sources", "none"]).unwrap_or(0),
                        value_usize(window, "frontier_exhausted").unwrap_or(0),
                    ));
                }
            }
        }
    }

    lines.join("\n")
}

fn floor_to_window(ts: DateTime<FixedOffset>, window_minutes: u32) -> DateTime<FixedOffset> {
    let floored_minute = (ts.minute() / window_minutes) * window_minutes;
    let seconds = ts.second() as i64;
    let nanos = ts.nanosecond() as i64;
    ts - Duration::seconds(seconds)
        - Duration::nanoseconds(nanos)
        - Duration::minutes((ts.minute() - floored_minute) as i64)
}

fn p90_nonzero(values: &[f64]) -> f64 {
    let mut items = values
        .iter()
        .copied()
        .filter(|value| *value > 0.0)
        .collect::<Vec<_>>();
    if items.is_empty() {
        return 0.0;
    }
    items.sort_by(f64::total_cmp);
    let index = ((items.len().saturating_sub(1)) as f64 * 0.90).round() as usize;
    items[index.min(items.len().saturating_sub(1))]
}

fn relative_ratio(value: f64, baseline: f64) -> f64 {
    if baseline <= 0.0 {
        1.0
    } else {
        value / baseline
    }
}

fn count_episodes(windows: &[FamilyWindowSummary], target_status: &str) -> usize {
    let mut count = 0usize;
    let mut in_episode = false;
    for window in windows {
        if window.status == target_status {
            if !in_episode {
                count += 1;
                in_episode = true;
            }
        } else {
            in_episode = false;
        }
    }
    count
}

fn update_route_extrema(
    min_slot: &mut Option<usize>,
    max_slot: &mut Option<usize>,
    last_slot: &mut Option<usize>,
    value: Option<usize>,
) {
    let Some(value) = value else {
        return;
    };
    *last_slot = Some(value);
    *min_slot = Some(min_slot.map_or(value, |current| current.min(value)));
    *max_slot = Some(max_slot.map_or(value, |current| current.max(value)));
}

fn parse_event_ts(value: Option<&Value>) -> Result<Option<DateTime<FixedOffset>>, String> {
    let Some(value) = value.and_then(Value::as_str) else {
        return Ok(None);
    };
    DateTime::parse_from_rfc3339(value)
        .map(Some)
        .map_err(|error| format!("Invalid timestamp '{}': {}", value, error))
}

fn field_as_string(fields: &Map<String, Value>, key: &str) -> Option<String> {
    fields.get(key).and_then(Value::as_str).map(str::to_string)
}

fn field_as_f64(fields: &Map<String, Value>, key: &str) -> Option<f64> {
    fields.get(key).and_then(Value::as_f64)
}

fn field_as_usize(fields: &Map<String, Value>, key: &str) -> Option<usize> {
    fields
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
}

fn value_usize(data: &Value, key: &str) -> Option<usize> {
    data.get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
}

fn value_path_usize(value: &Value, path: &[&str]) -> Option<usize> {
    value_path(value, path)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
}

fn value_path<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter()
        .try_fold(value, |current, segment| current.get(*segment))
}

fn format_optional_float(value: Option<f64>) -> String {
    value
        .map(|value| {
            if value.fract() == 0.0 {
                format!("{:.0}", value)
            } else {
                format!("{:.1}", value)
            }
        })
        .unwrap_or_else(|| "n/a".to_string())
}

fn format_optional_usize(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "n/a".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn classify_window_marks_clear_collapse() {
        assert_eq!(
            classify_window(0.0, 0.0, 0.0, 0.0, 10, 10, 100.0, 80.0, 400.0, 8.0),
            "collapsed"
        );
    }

    #[test]
    fn floor_to_window_rounds_down_to_requested_minutes() {
        let ts = DateTime::parse_from_rfc3339("2026-04-12T12:17:54-04:00").unwrap();
        let floored = floor_to_window(ts, 5);
        assert_eq!(floored.to_rfc3339(), "2026-04-12T12:15:00-04:00");
    }

    #[test]
    fn analyze_dht_stability_renders_first_value_sources() {
        let dir = tempdir().expect("create tempdir");
        let path = dir.path().join("metrics.jsonl");
        fs::write(
            &path,
            [
                r#"{"ts":"2026-04-12T12:00:05-04:00","session_id":"session-a","event_type":"dht_internal_family_summary","fields":{"purpose":"lookup","family":"ipv4","active_routes_available":100,"query_successes":40,"peers":300,"unique_peers_before_first_batch":10,"first_value_source":"seed","ended_reason":"peer_limit_reached"}}"#,
                r#"{"ts":"2026-04-12T12:00:15-04:00","session_id":"session-a","event_type":"dht_internal_family_summary","fields":{"purpose":"lookup","family":"ipv4","active_routes_available":100,"query_successes":42,"peers":310,"unique_peers_before_first_batch":11,"first_value_source":"discovered","ended_reason":"peer_limit_reached"}}"#,
            ]
            .join("\n"),
        )
        .expect("write metrics");

        let analysis =
            analyze_dht_stability(&path, &DhtStabilityAnalysisOptions { window_minutes: 1 })
                .expect("analyze stability");

        let families = analysis
            .data
            .get("families")
            .and_then(Value::as_array)
            .expect("families array");
        let ipv4 = families
            .iter()
            .find(|family| family.get("family").and_then(Value::as_str) == Some("ipv4"))
            .expect("ipv4 family");
        let windows = ipv4
            .get("windows")
            .and_then(Value::as_array)
            .expect("windows array");
        let first_window = windows.first().expect("window");
        assert_eq!(
            value_path_usize(first_window, &["first_value_sources", "seed"]),
            Some(1)
        );
        assert_eq!(
            value_path_usize(first_window, &["first_value_sources", "discovered"]),
            Some(1)
        );
    }
}
