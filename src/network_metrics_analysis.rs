// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later
// TEMP-BENCHMARK-ONLY: remove this temporary analyzer before pushing.

use chrono::{DateTime, FixedOffset};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct NetworkMetricsAnalysisOptions {
    pub info_hash: Option<String>,
    pub from_ts: Option<String>,
    pub to_ts: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NetworkMetricsAnalysis {
    pub text: String,
    pub data: Value,
}

#[derive(Debug, Clone, Default)]
struct DhtLookupAccumulator {
    first_batch_ms: Option<f64>,
    peers: usize,
    started: bool,
}

#[derive(Debug, Clone)]
struct InternalFamilyAccumulator {
    summaries: Vec<Map<String, Value>>,
    end_reasons: HashMap<String, usize>,
}

pub fn analyze_network_metrics(
    path: &Path,
    options: &NetworkMetricsAnalysisOptions,
) -> Result<NetworkMetricsAnalysis, String> {
    let from_ts = parse_optional_ts(options.from_ts.as_deref())?;
    let to_ts = parse_optional_ts(options.to_ts.as_deref())?;
    let info_hash_filter = options
        .info_hash
        .as_ref()
        .map(|value| value.to_ascii_lowercase());

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

    let mut candidates_by_source: BTreeMap<String, usize> = BTreeMap::new();
    let mut unique_candidates_by_source: BTreeMap<String, HashSet<String>> = BTreeMap::new();
    let mut discovery_mix: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();

    let mut attempts_by_source: BTreeMap<String, usize> = BTreeMap::new();
    let mut successes_by_source: BTreeMap<String, usize> = BTreeMap::new();
    let mut permit_waits_ms = Vec::new();
    let mut tcp_connect_ms = Vec::new();

    let mut tracker_started: BTreeMap<String, usize> = BTreeMap::new();
    let mut tracker_completed: BTreeMap<String, usize> = BTreeMap::new();
    let mut tracker_failed: BTreeMap<String, usize> = BTreeMap::new();

    let mut failure_reasons: HashMap<String, usize> = HashMap::new();
    let mut dropped_events = 0u64;

    let mut inbound_accepted = 0usize;
    let mut inbound_routed = 0usize;
    let mut port_open_marked = 0usize;

    let mut dht_lookups: HashMap<String, DhtLookupAccumulator> = HashMap::new();
    let mut internal_family_summaries: BTreeMap<(String, String), InternalFamilyAccumulator> =
        BTreeMap::new();

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

        let event_ts = parse_event_ts(event_object.get("ts"))?;
        if let Some(lower) = from_ts {
            if let Some(ts) = event_ts {
                if ts < lower {
                    continue;
                }
            }
        }
        if let Some(upper) = to_ts {
            if let Some(ts) = event_ts {
                if ts > upper {
                    continue;
                }
            }
        }
        if let Some(filter) = info_hash_filter.as_deref() {
            if event_object
                .get("info_hash")
                .and_then(Value::as_str)
                .map(|value| value.eq_ignore_ascii_case(filter))
                != Some(true)
            {
                continue;
            }
        }

        total_events += 1;
        if let Some(session_id) = event_object.get("session_id").and_then(Value::as_str) {
            session_ids.insert(session_id.to_string());
        }
        if let Some(ts) = event_ts {
            first_ts = Some(first_ts.map_or(ts, |current| current.min(ts)));
            last_ts = Some(last_ts.map_or(ts, |current| current.max(ts)));
        }

        let event_type = event_object
            .get("event_type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let source = event_object
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let peer_addr = event_object.get("peer_addr").and_then(Value::as_str);
        let address_family = event_object.get("address_family").and_then(Value::as_str);
        let fields = event_object
            .get("fields")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        match event_type {
            "peer_candidate_discovered" => {
                *candidates_by_source.entry(source.clone()).or_default() += 1;
                if let Some(peer_addr) = peer_addr {
                    unique_candidates_by_source
                        .entry(source.clone())
                        .or_default()
                        .insert(peer_addr.to_string());
                }
                if let Some(address_family) = address_family {
                    *discovery_mix
                        .entry(source.clone())
                        .or_default()
                        .entry(address_family.to_string())
                        .or_default() += 1;
                }
            }
            "outgoing_connect_requested" => {
                *attempts_by_source.entry(source.clone()).or_default() += 1;
            }
            "outgoing_tcp_connect_succeeded" => {
                *successes_by_source.entry(source.clone()).or_default() += 1;
                if let Some(elapsed_ms) = field_as_f64(&fields, "elapsed_ms") {
                    tcp_connect_ms.push(elapsed_ms);
                }
            }
            "outgoing_tcp_connect_failed" => {
                if let Some(elapsed_ms) = field_as_f64(&fields, "elapsed_ms") {
                    tcp_connect_ms.push(elapsed_ms);
                }
                if let Some(reason) = field_as_string(&fields, "reason") {
                    *failure_reasons.entry(reason).or_default() += 1;
                }
            }
            "outgoing_permit_wait" => {
                if let Some(elapsed_ms) = field_as_f64(&fields, "elapsed_ms") {
                    permit_waits_ms.push(elapsed_ms);
                }
            }
            "peer_backoff_applied" => {
                *failure_reasons
                    .entry("peer_backoff_applied".to_string())
                    .or_default() += 1;
            }
            "peer_session_ended" => {
                if let Some(reason) = field_as_string(&fields, "reason") {
                    *failure_reasons.entry(reason).or_default() += 1;
                }
            }
            "tracker_announce_started" => {
                let scheme = field_as_string(&fields, "scheme").unwrap_or_else(|| "unknown".into());
                *tracker_started.entry(scheme).or_default() += 1;
            }
            "tracker_announce_completed" => {
                let scheme = field_as_string(&fields, "scheme").unwrap_or_else(|| "unknown".into());
                *tracker_completed.entry(scheme).or_default() += 1;
            }
            "tracker_announce_failed" => {
                let scheme = field_as_string(&fields, "scheme").unwrap_or_else(|| "unknown".into());
                *tracker_failed.entry(scheme).or_default() += 1;
                if let Some(reason) = field_as_string(&fields, "failure_category") {
                    *failure_reasons.entry(reason).or_default() += 1;
                }
            }
            "instrumentation_dropped" => {
                if let Some(total) = field_as_u64(&fields, "dropped_events_total") {
                    dropped_events = dropped_events.max(total);
                }
            }
            "inbound_tcp_accepted" => inbound_accepted += 1,
            "inbound_peer_routed" => inbound_routed += 1,
            "port_open_marked" => port_open_marked += 1,
            "dht_internal_family_summary" => {
                let purpose =
                    field_as_string(&fields, "purpose").unwrap_or_else(|| "unknown".into());
                let family = field_as_string(&fields, "family").unwrap_or_else(|| "unknown".into());
                let ended_reason =
                    field_as_string(&fields, "ended_reason").unwrap_or_else(|| "unknown".into());
                let entry = internal_family_summaries
                    .entry((purpose, family))
                    .or_insert_with(|| InternalFamilyAccumulator {
                        summaries: Vec::new(),
                        end_reasons: HashMap::new(),
                    });
                entry.summaries.push(fields.clone());
                *entry.end_reasons.entry(ended_reason).or_default() += 1;
            }
            _ => {}
        }

        if event_type.starts_with("dht_lookup_") {
            if let Some(lookup_id) = field_as_string(&fields, "lookup_id") {
                let lookup = dht_lookups.entry(lookup_id).or_default();
                match event_type {
                    "dht_lookup_started" => lookup.started = true,
                    "dht_lookup_batch" => {
                        if lookup.first_batch_ms.is_none() {
                            lookup.first_batch_ms = field_as_f64(&fields, "elapsed_ms");
                        }
                        lookup.peers += field_as_usize(&fields, "batch_size").unwrap_or(0);
                    }
                    _ => {}
                }
            }
        }
    }

    let data = build_summary_json(
        total_events,
        session_ids,
        first_ts,
        last_ts,
        candidates_by_source,
        unique_candidates_by_source,
        discovery_mix,
        attempts_by_source,
        successes_by_source,
        permit_waits_ms,
        tcp_connect_ms,
        tracker_started,
        tracker_completed,
        tracker_failed,
        failure_reasons,
        dropped_events,
        inbound_accepted,
        inbound_routed,
        port_open_marked,
        dht_lookups,
        internal_family_summaries,
    );
    let text = render_text(&data);

    Ok(NetworkMetricsAnalysis { text, data })
}

#[allow(clippy::too_many_arguments)]
fn build_summary_json(
    total_events: usize,
    session_ids: BTreeSet<String>,
    first_ts: Option<DateTime<FixedOffset>>,
    last_ts: Option<DateTime<FixedOffset>>,
    candidates_by_source: BTreeMap<String, usize>,
    unique_candidates_by_source: BTreeMap<String, HashSet<String>>,
    discovery_mix: BTreeMap<String, BTreeMap<String, usize>>,
    attempts_by_source: BTreeMap<String, usize>,
    successes_by_source: BTreeMap<String, usize>,
    permit_waits_ms: Vec<f64>,
    tcp_connect_ms: Vec<f64>,
    tracker_started: BTreeMap<String, usize>,
    tracker_completed: BTreeMap<String, usize>,
    tracker_failed: BTreeMap<String, usize>,
    failure_reasons: HashMap<String, usize>,
    dropped_events: u64,
    inbound_accepted: usize,
    inbound_routed: usize,
    port_open_marked: usize,
    dht_lookups: HashMap<String, DhtLookupAccumulator>,
    internal_family_summaries: BTreeMap<(String, String), InternalFamilyAccumulator>,
) -> Value {
    let dht_first_batch_ms = dht_lookups
        .values()
        .filter_map(|lookup| lookup.first_batch_ms)
        .collect::<Vec<_>>();
    let dht_peers_per_lookup = dht_lookups
        .values()
        .filter(|lookup| lookup.started)
        .map(|lookup| lookup.peers as f64)
        .collect::<Vec<_>>();

    let session_summary = if session_ids.is_empty() {
        Value::Null
    } else {
        let duration_seconds = match (first_ts, last_ts) {
            (Some(start), Some(end)) => Some((end - start).num_milliseconds() as f64 / 1000.0),
            _ => None,
        };
        json!({
            "unique_sessions": session_ids.len(),
            "session_id": (session_ids.len() == 1).then(|| session_ids.iter().next().cloned()).flatten(),
            "start": first_ts.map(|ts| ts.to_rfc3339()),
            "end": last_ts.map(|ts| ts.to_rfc3339()),
            "duration_seconds": duration_seconds,
        })
    };

    let candidates_by_source = candidates_by_source
        .into_iter()
        .map(|(source, total)| {
            json!({
                "source": source,
                "total": total,
                "unique": unique_candidates_by_source.get(&source).map(|set| set.len()).unwrap_or(0),
            })
        })
        .collect::<Vec<_>>();

    let all_sources = attempts_by_source
        .keys()
        .chain(successes_by_source.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let outgoing_connections_by_source = all_sources
        .into_iter()
        .map(|source| {
            let attempts = attempts_by_source.get(&source).copied().unwrap_or(0);
            let successes = successes_by_source.get(&source).copied().unwrap_or(0);
            json!({
                "source": source,
                "attempts": attempts,
                "successes": successes,
                "success_rate": percentage(successes, attempts),
            })
        })
        .collect::<Vec<_>>();

    let internal_dht_family_summaries = internal_family_summaries
        .into_iter()
        .map(|((purpose, family), accumulator)| {
            let visit_cap_hits = accumulator
                .summaries
                .iter()
                .filter(|entry| entry.get("visit_cap_reached").and_then(Value::as_bool) == Some(true))
                .count();
            let peer_cap_hits = accumulator
                .summaries
                .iter()
                .filter(|entry| entry.get("peer_cap_reached").and_then(Value::as_bool) == Some(true))
                .count();
            json!({
                "purpose": purpose,
                "family": family,
                "count": accumulator.summaries.len(),
                "active_routes_available_avg": mean_field(&accumulator.summaries, "active_routes_available"),
                "cached_routes_available_avg": mean_field(&accumulator.summaries, "cached_routes_available"),
                "seeded_total_avg": mean_field(&accumulator.summaries, "seeded_total"),
                "seeded_bootstrap_avg": mean_field(&accumulator.summaries, "seeded_bootstrap"),
                "seeded_cached_avg": mean_field(&accumulator.summaries, "seeded_cached"),
                "initial_wave_avg": mean_field(&accumulator.summaries, "initial_wave_limit"),
                "visited_avg": mean_field(&accumulator.summaries, "visited"),
                "query_success_avg": mean_field(&accumulator.summaries, "query_successes"),
                "query_failure_avg": mean_field(&accumulator.summaries, "query_failures"),
                "peer_values_seen_avg": mean_field(&accumulator.summaries, "peer_values_seen"),
                "peer_values_before_first_batch_avg": mean_field(&accumulator.summaries, "peer_values_before_first_batch"),
                "unique_peers_before_first_batch_avg": mean_field(&accumulator.summaries, "unique_peers_before_first_batch"),
                "duplicate_peers_before_first_batch_avg": mean_field(&accumulator.summaries, "duplicate_peers_before_first_batch"),
                "peer_values_after_first_batch_avg": mean_field(&accumulator.summaries, "peer_values_after_first_batch"),
                "unique_peers_after_first_batch_avg": mean_field(&accumulator.summaries, "unique_peers_after_first_batch"),
                "duplicate_peers_after_first_batch_avg": mean_field(&accumulator.summaries, "duplicate_peers_after_first_batch"),
                "responses_with_peers_avg": mean_field(&accumulator.summaries, "responses_with_peers"),
                "responses_with_peers_before_first_batch_avg": mean_field(&accumulator.summaries, "responses_with_peers_before_first_batch"),
                "responses_with_peers_after_first_batch_avg": mean_field(&accumulator.summaries, "responses_with_peers_after_first_batch"),
                "nodes_discovered_avg": mean_field(&accumulator.summaries, "nodes_discovered"),
                "nodes_accepted_avg": mean_field(&accumulator.summaries, "nodes_accepted"),
                "nodes_rejected_avg": mean_field(&accumulator.summaries, "nodes_rejected"),
                "peers_avg": mean_field(&accumulator.summaries, "peers"),
                "max_pending_p95": percentile_field(&accumulator.summaries, "max_pending", 95),
                "first_value_source_counts": count_string_field_values(&accumulator.summaries, "first_value_source", &["bootstrap", "seed", "discovered", "none"]),
                "visit_cap_hits": visit_cap_hits,
                "peer_cap_hits": peer_cap_hits,
                "end_reasons": sort_reason_counts(accumulator.end_reasons),
            })
        })
        .collect::<Vec<_>>();

    let discovery_mix_by_source = discovery_mix
        .into_iter()
        .map(|(source, families)| {
            json!({
                "source": source,
                "ipv4": families.get("ipv4").copied().unwrap_or(0),
                "ipv6": families.get("ipv6").copied().unwrap_or(0),
            })
        })
        .collect::<Vec<_>>();

    let tracker_schemes = tracker_started
        .keys()
        .chain(tracker_completed.keys())
        .chain(tracker_failed.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let tracker_announces = tracker_schemes
        .into_iter()
        .map(|scheme| {
            json!({
                "scheme": scheme,
                "started": tracker_started.get(&scheme).copied().unwrap_or(0),
                "completed": tracker_completed.get(&scheme).copied().unwrap_or(0),
                "failed": tracker_failed.get(&scheme).copied().unwrap_or(0),
            })
        })
        .collect::<Vec<_>>();

    let top_failure_reasons = sort_reason_counts(failure_reasons)
        .into_iter()
        .take(10)
        .collect::<Vec<_>>();

    let tracker_sources = ["tracker_http", "tracker_udp", "tracker_other"];
    let tracker_candidates = tracker_sources
        .iter()
        .map(|source| candidates_value(&candidates_by_source, source))
        .sum::<usize>();
    let tracker_attempts = tracker_sources
        .iter()
        .map(|source| attempts_by_source.get(*source).copied().unwrap_or(0))
        .sum::<usize>();
    let tracker_successes = tracker_sources
        .iter()
        .map(|source| successes_by_source.get(*source).copied().unwrap_or(0))
        .sum::<usize>();
    let dht_attempts = attempts_by_source.get("dht").copied().unwrap_or(0);
    let dht_successes = successes_by_source.get("dht").copied().unwrap_or(0);

    json!({
        "events": total_events,
        "session_summary": session_summary,
        "candidates_by_source": candidates_by_source,
        "outgoing_connections_by_source": outgoing_connections_by_source,
        "outgoing_permit_wait": {
            "avg_ms": mean(&permit_waits_ms),
            "p95_ms": percentile(&permit_waits_ms, 95),
        },
        "tcp_connect": {
            "avg_ms": mean(&tcp_connect_ms),
            "p95_ms": percentile(&tcp_connect_ms, 95),
        },
        "dht_lookups": {
            "count": dht_lookups.len(),
            "time_to_first_batch": {
                "avg_ms": mean(&dht_first_batch_ms),
                "p95_ms": percentile(&dht_first_batch_ms, 95),
            },
            "peers_per_lookup_avg": mean(&dht_peers_per_lookup),
            "peers_per_lookup_p95": percentile(&dht_peers_per_lookup, 95),
        },
        "internal_dht_family_summaries": internal_dht_family_summaries,
        "discovery_mix_by_source": discovery_mix_by_source,
        "tracker_announces": tracker_announces,
        "top_failure_reasons": top_failure_reasons,
        "dropped_instrumentation_events": dropped_events,
        "dht_vs_tracker": {
            "dht": {
                "candidates": candidates_value(&candidates_by_source, "dht"),
                "attempts": dht_attempts,
                "successes": dht_successes,
                "success_rate": percentage(dht_successes, dht_attempts),
            },
            "tracker": {
                "candidates": tracker_candidates,
                "attempts": tracker_attempts,
                "successes": tracker_successes,
                "success_rate": percentage(tracker_successes, tracker_attempts),
            },
        },
        "inbound_summary": {
            "accepted": inbound_accepted,
            "routed": inbound_routed,
            "port_open_marked": port_open_marked,
        }
    })
}

pub fn render_text(data: &Value) -> String {
    let mut lines = Vec::new();
    lines.push("Network Metrics Summary".to_string());
    lines.push(format!(
        "events: {}",
        value_usize(data, "events").unwrap_or(0)
    ));
    lines.push(String::new());

    if let Some(session) = data.get("session_summary").filter(|value| !value.is_null()) {
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

    lines.push("Candidates by source:".to_string());
    if let Some(items) = data.get("candidates_by_source").and_then(Value::as_array) {
        for item in items {
            lines.push(format!(
                "  {}: total={} unique={}",
                item.get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                value_usize(item, "total").unwrap_or(0),
                value_usize(item, "unique").unwrap_or(0)
            ));
        }
    }
    lines.push(String::new());

    lines.push("Outgoing connections by source:".to_string());
    if let Some(items) = data
        .get("outgoing_connections_by_source")
        .and_then(Value::as_array)
    {
        for item in items {
            lines.push(format!(
                "  {}: attempts={} successes={} success_rate={:.2}%",
                item.get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                value_usize(item, "attempts").unwrap_or(0),
                value_usize(item, "successes").unwrap_or(0),
                item.get("success_rate")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
            ));
        }
    }
    lines.push(String::new());

    lines.push(format!(
        "Outgoing permit wait: avg={} p95={}",
        format_ms_path(data, &["outgoing_permit_wait", "avg_ms"]),
        format_ms_path(data, &["outgoing_permit_wait", "p95_ms"])
    ));
    lines.push(format!(
        "TCP connect: avg={} p95={}",
        format_ms_path(data, &["tcp_connect", "avg_ms"]),
        format_ms_path(data, &["tcp_connect", "p95_ms"])
    ));
    lines.push(String::new());

    let dht_lookups = data.get("dht_lookups").cloned().unwrap_or(Value::Null);
    lines.push(format!(
        "DHT lookups: count={}",
        value_usize(&dht_lookups, "count").unwrap_or(0)
    ));
    lines.push(format!(
        "DHT time to first batch: avg={} p95={}",
        format_ms_path(&dht_lookups, &["time_to_first_batch", "avg_ms"]),
        format_ms_path(&dht_lookups, &["time_to_first_batch", "p95_ms"])
    ));
    lines.push(format!(
        "DHT peers per lookup: avg={} p95={}",
        format_optional_float(
            dht_lookups
                .get("peers_per_lookup_avg")
                .and_then(Value::as_f64)
        ),
        format_optional_float(
            dht_lookups
                .get("peers_per_lookup_p95")
                .and_then(Value::as_f64)
        )
    ));
    lines.push(String::new());

    if let Some(items) = data
        .get("internal_dht_family_summaries")
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
    {
        lines.push("Internal DHT family summaries:".to_string());
        for item in items {
            lines.push(format!(
                "  {}/{}: count={} active_routes_available_avg={} cached_routes_available_avg={} seeded_total_avg={} seeded_bootstrap_avg={} seeded_cached_avg={} initial_wave_avg={} visited_avg={} query_success_avg={} query_failure_avg={} peer_values_seen_avg={} peer_values_before_first_batch_avg={} unique_peers_before_first_batch_avg={} duplicate_peers_before_first_batch_avg={} peer_values_after_first_batch_avg={} unique_peers_after_first_batch_avg={} duplicate_peers_after_first_batch_avg={} responses_with_peers_avg={} responses_with_peers_before_first_batch_avg={} responses_with_peers_after_first_batch_avg={} nodes_discovered_avg={} nodes_accepted_avg={} nodes_rejected_avg={} peers_avg={} max_pending_p95={} visit_cap_hits={} peer_cap_hits={}",
                item.get("purpose").and_then(Value::as_str).unwrap_or("unknown"),
                item.get("family").and_then(Value::as_str).unwrap_or("unknown"),
                value_usize(item, "count").unwrap_or(0),
                format_optional_float(item.get("active_routes_available_avg").and_then(Value::as_f64)),
                format_optional_float(item.get("cached_routes_available_avg").and_then(Value::as_f64)),
                format_optional_float(item.get("seeded_total_avg").and_then(Value::as_f64)),
                format_optional_float(item.get("seeded_bootstrap_avg").and_then(Value::as_f64)),
                format_optional_float(item.get("seeded_cached_avg").and_then(Value::as_f64)),
                format_optional_float(item.get("initial_wave_avg").and_then(Value::as_f64)),
                format_optional_float(item.get("visited_avg").and_then(Value::as_f64)),
                format_optional_float(item.get("query_success_avg").and_then(Value::as_f64)),
                format_optional_float(item.get("query_failure_avg").and_then(Value::as_f64)),
                format_optional_float(item.get("peer_values_seen_avg").and_then(Value::as_f64)),
                format_optional_float(item.get("peer_values_before_first_batch_avg").and_then(Value::as_f64)),
                format_optional_float(item.get("unique_peers_before_first_batch_avg").and_then(Value::as_f64)),
                format_optional_float(item.get("duplicate_peers_before_first_batch_avg").and_then(Value::as_f64)),
                format_optional_float(item.get("peer_values_after_first_batch_avg").and_then(Value::as_f64)),
                format_optional_float(item.get("unique_peers_after_first_batch_avg").and_then(Value::as_f64)),
                format_optional_float(item.get("duplicate_peers_after_first_batch_avg").and_then(Value::as_f64)),
                format_optional_float(item.get("responses_with_peers_avg").and_then(Value::as_f64)),
                format_optional_float(item.get("responses_with_peers_before_first_batch_avg").and_then(Value::as_f64)),
                format_optional_float(item.get("responses_with_peers_after_first_batch_avg").and_then(Value::as_f64)),
                format_optional_float(item.get("nodes_discovered_avg").and_then(Value::as_f64)),
                format_optional_float(item.get("nodes_accepted_avg").and_then(Value::as_f64)),
                format_optional_float(item.get("nodes_rejected_avg").and_then(Value::as_f64)),
                format_optional_float(item.get("peers_avg").and_then(Value::as_f64)),
                format_optional_float(item.get("max_pending_p95").and_then(Value::as_f64)),
                value_usize(item, "visit_cap_hits").unwrap_or(0),
                value_usize(item, "peer_cap_hits").unwrap_or(0),
            ));
            if let Some(first_value_counts) = item
                .get("first_value_source_counts")
                .and_then(Value::as_array)
            {
                let rendered = first_value_counts
                    .iter()
                    .map(|entry| {
                        format!(
                            "{}={}",
                            entry
                                .get("source")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown"),
                            value_usize(entry, "count").unwrap_or(0)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                lines.push(format!("    first_value_sources: {}", rendered));
            }
            let reasons = item
                .get("end_reasons")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .map(|reason| {
                            format!(
                                "{}={}",
                                reason
                                    .get("reason")
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown"),
                                value_usize(reason, "count").unwrap_or(0)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            lines.push(format!("    end_reasons: {}", reasons));
        }
        lines.push(String::new());
    }

    lines.push("Discovery mix by source:".to_string());
    if let Some(items) = data
        .get("discovery_mix_by_source")
        .and_then(Value::as_array)
    {
        for item in items {
            lines.push(format!(
                "  {}: ipv4={} ipv6={}",
                item.get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                value_usize(item, "ipv4").unwrap_or(0),
                value_usize(item, "ipv6").unwrap_or(0)
            ));
        }
    }
    lines.push(String::new());

    lines.push("Tracker announces:".to_string());
    if let Some(items) = data.get("tracker_announces").and_then(Value::as_array) {
        for item in items {
            lines.push(format!(
                "  {}: started={} completed={} failed={}",
                item.get("scheme")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                value_usize(item, "started").unwrap_or(0),
                value_usize(item, "completed").unwrap_or(0),
                value_usize(item, "failed").unwrap_or(0)
            ));
        }
    }
    lines.push(String::new());

    lines.push("Top failure reasons:".to_string());
    if let Some(items) = data.get("top_failure_reasons").and_then(Value::as_array) {
        for item in items {
            lines.push(format!(
                "  {}: {}",
                item.get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                value_usize(item, "count").unwrap_or(0)
            ));
        }
    }
    lines.push(String::new());

    lines.push(format!(
        "Dropped instrumentation events: {}",
        value_u64(data, "dropped_instrumentation_events").unwrap_or(0)
    ));
    lines.push(String::new());

    let dht_vs_tracker = data.get("dht_vs_tracker").cloned().unwrap_or(Value::Null);
    lines.push("DHT vs tracker:".to_string());
    lines.push(format!(
        "  dht: candidates={} attempts={} successes={} success_rate={:.2}%",
        value_path_usize(&dht_vs_tracker, &["dht", "candidates"]).unwrap_or(0),
        value_path_usize(&dht_vs_tracker, &["dht", "attempts"]).unwrap_or(0),
        value_path_usize(&dht_vs_tracker, &["dht", "successes"]).unwrap_or(0),
        value_path_f64(&dht_vs_tracker, &["dht", "success_rate"]).unwrap_or(0.0)
    ));
    lines.push(format!(
        "  tracker: candidates={} attempts={} successes={} success_rate={:.2}%",
        value_path_usize(&dht_vs_tracker, &["tracker", "candidates"]).unwrap_or(0),
        value_path_usize(&dht_vs_tracker, &["tracker", "attempts"]).unwrap_or(0),
        value_path_usize(&dht_vs_tracker, &["tracker", "successes"]).unwrap_or(0),
        value_path_f64(&dht_vs_tracker, &["tracker", "success_rate"]).unwrap_or(0.0)
    ));
    lines.push(String::new());

    let inbound = data.get("inbound_summary").cloned().unwrap_or(Value::Null);
    lines.push("Inbound summary:".to_string());
    lines.push(format!(
        "  accepted={}",
        value_usize(&inbound, "accepted").unwrap_or(0)
    ));
    lines.push(format!(
        "  routed={}",
        value_usize(&inbound, "routed").unwrap_or(0)
    ));
    lines.push(format!(
        "  port_open_marked={}",
        value_usize(&inbound, "port_open_marked").unwrap_or(0)
    ));

    lines.join("\n")
}

fn parse_optional_ts(value: Option<&str>) -> Result<Option<DateTime<FixedOffset>>, String> {
    value
        .map(|value| {
            DateTime::parse_from_rfc3339(value)
                .map_err(|error| format!("Invalid timestamp '{}': {}", value, error))
        })
        .transpose()
}

fn parse_event_ts(value: Option<&Value>) -> Result<Option<DateTime<FixedOffset>>, String> {
    match value.and_then(Value::as_str) {
        Some(value) => DateTime::parse_from_rfc3339(value)
            .map(Some)
            .map_err(|error| format!("Invalid event timestamp '{}': {}", value, error)),
        None => Ok(None),
    }
}

fn field_as_f64(fields: &Map<String, Value>, key: &str) -> Option<f64> {
    fields.get(key).and_then(|value| match value {
        Value::Number(number) => number.as_f64(),
        _ => None,
    })
}

fn field_as_usize(fields: &Map<String, Value>, key: &str) -> Option<usize> {
    fields.get(key).and_then(|value| match value {
        Value::Number(number) => number
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .or_else(|| {
                number
                    .as_i64()
                    .and_then(|value| usize::try_from(value).ok())
            }),
        _ => None,
    })
}

fn field_as_u64(fields: &Map<String, Value>, key: &str) -> Option<u64> {
    fields.get(key).and_then(|value| match value {
        Value::Number(number) => number.as_u64(),
        _ => None,
    })
}

fn field_as_string(fields: &Map<String, Value>, key: &str) -> Option<String> {
    fields.get(key).and_then(Value::as_str).map(str::to_string)
}

fn mean(values: &[f64]) -> Option<f64> {
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

fn percentile(values: &[f64], pct: usize) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut ordered = values.to_vec();
    ordered.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let index = (((pct as f64 / 100.0) * ordered.len() as f64).ceil() as usize)
        .saturating_sub(1)
        .min(ordered.len().saturating_sub(1));
    ordered.get(index).copied()
}

fn mean_field(entries: &[Map<String, Value>], key: &str) -> Option<f64> {
    let values = entries
        .iter()
        .filter_map(|entry| field_as_f64(entry, key))
        .collect::<Vec<_>>();
    mean(&values)
}

fn percentile_field(entries: &[Map<String, Value>], key: &str, pct: usize) -> Option<f64> {
    let values = entries
        .iter()
        .filter_map(|entry| field_as_f64(entry, key))
        .collect::<Vec<_>>();
    percentile(&values, pct)
}

fn count_string_field_values(
    entries: &[Map<String, Value>],
    key: &str,
    expected_values: &[&str],
) -> Vec<Value> {
    let mut counts = expected_values
        .iter()
        .map(|value| ((*value).to_string(), 0usize))
        .collect::<BTreeMap<_, _>>();
    for entry in entries {
        if let Some(value) = field_as_string(entry, key) {
            *counts.entry(value).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .map(|(source, count)| json!({ "source": source, "count": count }))
        .collect()
}

fn percentage(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64 * 100.0
    }
}

fn sort_reason_counts(counts: HashMap<String, usize>) -> Vec<Value> {
    let mut reasons = counts.into_iter().collect::<Vec<_>>();
    reasons.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    reasons
        .into_iter()
        .map(|(reason, count)| json!({ "reason": reason, "count": count }))
        .collect()
}

fn candidates_value(values: &[Value], source: &str) -> usize {
    values
        .iter()
        .find(|item| item.get("source").and_then(Value::as_str) == Some(source))
        .and_then(|item| value_usize(item, "total"))
        .unwrap_or(0)
}

fn value_usize(value: &Value, key: &str) -> Option<usize> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
}

fn value_u64(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

fn value_path_usize(value: &Value, path: &[&str]) -> Option<usize> {
    value_path(value, path)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
}

fn value_path_f64(value: &Value, path: &[&str]) -> Option<f64> {
    value_path(value, path).and_then(Value::as_f64)
}

fn value_path<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter()
        .try_fold(value, |current, segment| current.get(*segment))
}

fn format_ms_path(value: &Value, path: &[&str]) -> String {
    format_ms(value_path_f64(value, path))
}

fn format_ms(value: Option<f64>) -> String {
    value
        .map(|value| format!("{:.2}ms", value))
        .unwrap_or_else(|| "n/a".to_string())
}

fn format_optional_float(value: Option<f64>) -> String {
    value
        .map(|value| format!("{:.2}", value))
        .unwrap_or_else(|| "n/a".to_string())
}

#[cfg(test)]
mod tests {
    use super::{analyze_network_metrics, NetworkMetricsAnalysisOptions};
    use serde_json::Value;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn analyze_network_metrics_summarizes_sample_file() {
        let dir = tempdir().expect("create tempdir");
        let path = dir.path().join("metrics.jsonl");
        fs::write(
            &path,
            [
                r#"{"ts":"2026-04-10T10:00:00-04:00","session_id":"session-a","event_type":"peer_candidate_discovered","info_hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","peer_addr":"1.1.1.1:6881","address_family":"ipv4","source":"dht","fields":{}}"#,
                r#"{"ts":"2026-04-10T10:00:01-04:00","session_id":"session-a","event_type":"outgoing_connect_requested","info_hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","source":"dht","fields":{}}"#,
                r#"{"ts":"2026-04-10T10:00:02-04:00","session_id":"session-a","event_type":"outgoing_tcp_connect_succeeded","info_hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","source":"dht","fields":{"elapsed_ms":125}}"#,
                r#"{"ts":"2026-04-10T10:00:03-04:00","session_id":"session-a","event_type":"dht_lookup_started","info_hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","fields":{"lookup_id":"lookup-1"}}"#,
                r#"{"ts":"2026-04-10T10:00:04-04:00","session_id":"session-a","event_type":"dht_lookup_batch","info_hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","fields":{"lookup_id":"lookup-1","elapsed_ms":50,"batch_size":8}}"#,
                r#"{"ts":"2026-04-10T10:00:05-04:00","session_id":"session-a","event_type":"dht_internal_family_summary","info_hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","fields":{"purpose":"lookup","family":"ipv4","visited":10,"query_successes":4,"query_failures":2,"peers":8,"max_pending":12,"first_value_source":"seed","ended_reason":"frontier_exhausted"}}"#,
            ]
            .join("\n"),
        )
        .expect("write metrics");

        let analysis = analyze_network_metrics(
            &path,
            &NetworkMetricsAnalysisOptions {
                info_hash: None,
                from_ts: None,
                to_ts: None,
            },
        )
        .expect("analyze metrics");

        assert_eq!(analysis.data.get("events").and_then(Value::as_u64), Some(6));
        assert!(analysis.text.contains("Network Metrics Summary"));
        assert!(analysis.text.contains("DHT lookups: count=1"));
        assert!(analysis.text.contains("lookup/ipv4"));
        assert!(analysis.text.contains("first_value_sources:"));
        assert!(analysis.text.contains("seed=1"));
    }
}
