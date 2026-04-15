// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Copy)]
pub struct DhtTraceCompareOptions {
    pub context_events: usize,
}

#[derive(Debug, Clone)]
pub struct DhtTraceComparison {
    pub text: String,
    pub data: Value,
}

#[derive(Debug, Clone)]
struct ParsedTraceLine {
    backend: String,
    line_number: usize,
    fields: BTreeMap<String, String>,
    raw: String,
}

#[derive(Debug, Clone)]
struct NormalizedTraceEvent {
    backend: String,
    line_number: usize,
    kind: String,
    target: Option<String>,
    summary: String,
    raw: String,
}

#[derive(Debug, Clone, Default)]
struct TraceHeader {
    backend: Option<String>,
    owner: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct RouteSummary {
    total: usize,
    inserted_or_added: usize,
    retained_or_rejected: usize,
    secure: usize,
    kinds: BTreeMap<String, usize>,
}

pub fn compare_dht_traces(
    left: &Path,
    right: &Path,
    options: &DhtTraceCompareOptions,
) -> Result<DhtTraceComparison, String> {
    let left_text = fs::read_to_string(left)
        .map_err(|error| format!("Failed to read trace '{}': {}", left.display(), error))?;
    let right_text = fs::read_to_string(right)
        .map_err(|error| format!("Failed to read trace '{}': {}", right.display(), error))?;

    let left_header = parse_trace_header(&left_text);
    let right_header = parse_trace_header(&right_text);
    let left_lines = parse_trace_lines(&left_text, left_header.backend.as_deref())?;
    let right_lines = parse_trace_lines(&right_text, right_header.backend.as_deref())?;

    let left_owner = left_header.owner.as_deref();
    let right_owner = right_header.owner.as_deref();
    let left_events = normalize_lookup_events(&left_lines, left_owner);
    let right_events = normalize_lookup_events(&right_lines, right_owner);
    let left_route_summary = summarize_route_updates(&left_lines, left_owner);
    let right_route_summary = summarize_route_updates(&right_lines, right_owner);

    let first_divergence = first_sequence_divergence(&left_events, &right_events);
    let preview = build_preview(
        &left_events,
        &right_events,
        first_divergence,
        options.context_events,
    );
    let left_counts = count_events(&left_events);
    let right_counts = count_events(&right_events);

    let text = render_text(
        left,
        right,
        &left_header,
        &right_header,
        &left_events,
        &right_events,
        &left_counts,
        &right_counts,
        &left_route_summary,
        &right_route_summary,
        first_divergence,
        &preview,
    );

    let data = json!({
        "left": {
            "path": left.display().to_string(),
            "header": {
                "backend": left_header.backend,
                "owner": left_header.owner,
            },
            "lookup_event_count": left_events.len(),
            "event_counts": left_counts,
            "route_summary": route_summary_json(&left_route_summary),
        },
        "right": {
            "path": right.display().to_string(),
            "header": {
                "backend": right_header.backend,
                "owner": right_header.owner,
            },
            "lookup_event_count": right_events.len(),
            "event_counts": right_counts,
            "route_summary": route_summary_json(&right_route_summary),
        },
        "first_divergence_index": first_divergence,
        "preview": preview.iter().map(|pair| {
            json!({
                "index": pair.0,
                "left": pair.1.as_ref().map(event_json),
                "right": pair.2.as_ref().map(event_json),
            })
        }).collect::<Vec<_>>(),
    });

    Ok(DhtTraceComparison { text, data })
}

fn parse_trace_header(raw: &str) -> TraceHeader {
    for line in raw.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("dht_trace_header ") {
            let fields = parse_key_values(rest);
            return TraceHeader {
                backend: fields.get("backend").cloned(),
                owner: fields.get("owner").cloned(),
            };
        }
    }
    TraceHeader::default()
}

fn parse_trace_lines(
    raw: &str,
    header_backend: Option<&str>,
) -> Result<Vec<ParsedTraceLine>, String> {
    let mut lines = Vec::new();
    let normalized_raw = normalize_trace_text(raw);

    for (index, line) in normalized_raw.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("dht_trace_header ") {
            continue;
        }

        let (backend, body) = if let Some(rest) = trimmed.strip_prefix("mainline_probe ") {
            ("mainline".to_string(), rest)
        } else if let Some(rest) = trimmed.strip_prefix("internal_probe ") {
            ("internal".to_string(), rest)
        } else if trimmed.starts_with("event=") {
            (
                normalize_backend_name(header_backend.unwrap_or("unknown")),
                trimmed,
            )
        } else {
            continue;
        };

        let fields = parse_key_values(body);
        if !fields.contains_key("event") {
            continue;
        }

        lines.push(ParsedTraceLine {
            backend,
            line_number: index + 1,
            fields,
            raw: trimmed.to_string(),
        });
    }

    Ok(lines)
}

fn normalize_backend_name(raw: &str) -> String {
    match raw {
        "internalprototype" => "internal".to_string(),
        other => other.to_string(),
    }
}

fn normalize_trace_text(raw: &str) -> String {
    let mut normalized = raw.replace("event=", "\nevent=");
    normalized = normalized.replace("mainline_probe \nevent=", "\nmainline_probe event=");
    normalized = normalized.replace("internal_probe \nevent=", "\ninternal_probe event=");
    if let Some(stripped) = normalized.strip_prefix('\n') {
        stripped.to_string()
    } else {
        normalized
    }
}

fn normalize_lookup_events(
    lines: &[ParsedTraceLine],
    owner_filter: Option<&str>,
) -> Vec<NormalizedTraceEvent> {
    let mut events = Vec::new();

    for line in lines {
        if !owner_matches(line, owner_filter) {
            continue;
        }

        let Some(event_name) = line.fields.get("event").map(String::as_str) else {
            continue;
        };

        let normalized = match (line.backend.as_str(), event_name) {
            ("mainline", "query_seeded") if field_contains(line, "request", "GetPeers") => {
                Some(NormalizedTraceEvent {
                    backend: line.backend.clone(),
                    line_number: line.line_number,
                    kind: "seed_state".to_string(),
                    target: line.fields.get("target").map(|value| normalize_target(value)),
                    summary: format!(
                        "rt={} selected={} bootstrap={} cached_responders={} subnets={} secure={}",
                        field(line, "routing_table_size"),
                        field(line, "routing_table_selected"),
                        field(line, "bootstrap_seeded"),
                        field(line, "cached_responders"),
                        field(line, "routing_table_selected_subnets"),
                        field(line, "routing_table_selected_secure"),
                    ),
                    raw: line.raw.clone(),
                })
            }
            ("mainline", "query_visit_round") if field_contains(line, "request", "GetPeers") => {
                Some(NormalizedTraceEvent {
                    backend: line.backend.clone(),
                    line_number: line.line_number,
                    kind: "visit_round".to_string(),
                    target: line.fields.get("target").map(|value| normalize_target(value)),
                    summary: format!(
                        "round={} batch={} seed_candidates={} discovered_candidates={} visited_before={}",
                        field(line, "round"),
                        field(line, "batch_size"),
                        field(line, "topk_seed_candidates"),
                        field(line, "topk_discovered_candidates"),
                        field(line, "visited_before"),
                    ),
                    raw: line.raw.clone(),
                })
            }
            ("mainline", "query_first_value") if field_contains(line, "request", "GetPeers") => {
                Some(NormalizedTraceEvent {
                    backend: line.backend.clone(),
                    line_number: line.line_number,
                    kind: "first_value".to_string(),
                    target: line.fields.get("target").map(|value| normalize_target(value)),
                    summary: format!(
                        "source={} from={} visited={} closest={} responders={}",
                        field(line, "source"),
                        field(line, "from"),
                        field(line, "visited"),
                        field(line, "closest"),
                        field(line, "responders"),
                    ),
                    raw: line.raw.clone(),
                })
            }
            ("mainline", "query_completed") if field_contains(line, "request", "GetPeers") => {
                Some(NormalizedTraceEvent {
                    backend: line.backend.clone(),
                    line_number: line.line_number,
                    kind: "query_completed".to_string(),
                    target: line.fields.get("target").map(|value| normalize_target(value)),
                    summary: format!(
                        "visited={} responders={} seed_visits={} discovered_visits={} first_value_source={}",
                        field(line, "visited"),
                        field(line, "responders"),
                        field(line, "seed_visits"),
                        field(line, "discovered_visits"),
                        field(line, "first_value_source"),
                    ),
                    raw: line.raw.clone(),
                })
            }
            ("internal", "query_seed_state") if field_equals(line, "family", "ipv4") => {
                Some(NormalizedTraceEvent {
                    backend: line.backend.clone(),
                    line_number: line.line_number,
                    kind: "seed_state".to_string(),
                    target: line.fields.get("target").cloned(),
                    summary: format!(
                        "active={} lookup_proven={} frontier={} cached={} bootstrap={} buckets={} bucketed={} overflow={}",
                        field(line, "active_total"),
                        field(line, "active_lookup_proven"),
                        field(line, "fast_frontier_available"),
                        field(line, "cached_seed_candidates"),
                        field(line, "bootstrap_candidates"),
                        field(line, "ipv4_bucket_count"),
                        field(line, "ipv4_bucketed_routes"),
                        field(line, "ipv4_overflow_routes"),
                    ),
                    raw: line.raw.clone(),
                })
            }
            ("internal", "query_visit_round")
                if field_equals(line, "family", "ipv4") && field_equals(line, "purpose", "lookup") =>
            {
                Some(NormalizedTraceEvent {
                    backend: line.backend.clone(),
                    line_number: line.line_number,
                    kind: "visit_round".to_string(),
                    target: line.fields.get("target").cloned(),
                    summary: format!(
                        "round={} batch={} bootstrap={} seed={} discovered={} visited_before={}",
                        field(line, "round"),
                        field(line, "batch_size"),
                        field(line, "bootstrap"),
                        field(line, "seed"),
                        field(line, "discovered"),
                        field(line, "visited_before"),
                    ),
                    raw: line.raw.clone(),
                })
            }
            ("internal", "query_first_value")
                if field_equals(line, "family", "ipv4") && field_equals(line, "purpose", "lookup") =>
            {
                Some(NormalizedTraceEvent {
                    backend: line.backend.clone(),
                    line_number: line.line_number,
                    kind: "first_value".to_string(),
                    target: line.fields.get("target").cloned(),
                    summary: format!(
                        "source={} from={} visited={} peers_before={} pending={}",
                        field(line, "source"),
                        field(line, "from"),
                        field(line, "visited"),
                        field(line, "peers_before"),
                        field(line, "pending"),
                    ),
                    raw: line.raw.clone(),
                })
            }
            ("internal", "query_completed")
                if field_equals(line, "family", "ipv4") && field_equals(line, "purpose", "lookup") =>
            {
                Some(NormalizedTraceEvent {
                    backend: line.backend.clone(),
                    line_number: line.line_number,
                    kind: "query_completed".to_string(),
                    target: line.fields.get("target").cloned(),
                    summary: format!(
                        "visited={} peers={} successes={} failures={} first_value_source={} ended_reason={}",
                        field(line, "visited"),
                        field(line, "peers"),
                        field(line, "query_successes"),
                        field(line, "query_failures"),
                        field(line, "first_value_source"),
                        field(line, "ended_reason"),
                    ),
                    raw: line.raw.clone(),
                })
            }
            _ => None,
        };

        if let Some(event) = normalized {
            events.push(event);
        }
    }

    events
}

fn summarize_route_updates(lines: &[ParsedTraceLine], owner_filter: Option<&str>) -> RouteSummary {
    let mut summary = RouteSummary::default();

    for line in lines {
        if !owner_matches(line, owner_filter) {
            continue;
        }

        let Some(event_name) = line.fields.get("event").map(String::as_str) else {
            continue;
        };

        match (line.backend.as_str(), event_name) {
            ("mainline", "route_admission") => {
                summary.total += 1;
                if field_equals(line, "added", "true") {
                    summary.inserted_or_added += 1;
                } else {
                    summary.retained_or_rejected += 1;
                }
                if field_equals(line, "secure", "true") {
                    summary.secure += 1;
                }
                *summary
                    .kinds
                    .entry(field(line, "source").to_string())
                    .or_default() += 1;
            }
            ("internal", "active_route_update") if field_equals(line, "family", "ipv4") => {
                summary.total += 1;
                if field_equals(line, "inserted", "true") {
                    summary.inserted_or_added += 1;
                } else {
                    summary.retained_or_rejected += 1;
                }
                *summary
                    .kinds
                    .entry(field(line, "kind").to_string())
                    .or_default() += 1;
            }
            _ => {}
        }
    }

    summary
}

fn first_sequence_divergence(
    left: &[NormalizedTraceEvent],
    right: &[NormalizedTraceEvent],
) -> Option<usize> {
    let common_len = left.len().min(right.len());
    for index in 0..common_len {
        if left[index].kind != right[index].kind {
            return Some(index);
        }
    }
    if left.len() != right.len() {
        Some(common_len)
    } else {
        None
    }
}

fn build_preview(
    left: &[NormalizedTraceEvent],
    right: &[NormalizedTraceEvent],
    first_divergence: Option<usize>,
    context_events: usize,
) -> Vec<(
    usize,
    Option<NormalizedTraceEvent>,
    Option<NormalizedTraceEvent>,
)> {
    let max_len = left.len().max(right.len());
    if max_len == 0 {
        return Vec::new();
    }

    let context = context_events.max(1);
    let start = first_divergence
        .unwrap_or(0)
        .saturating_sub(context / 2)
        .min(max_len.saturating_sub(1));
    let end = (start + context).min(max_len);

    (start..end)
        .map(|index| (index, left.get(index).cloned(), right.get(index).cloned()))
        .collect()
}

fn count_events(events: &[NormalizedTraceEvent]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for event in events {
        *counts.entry(event.kind.clone()).or_default() += 1;
    }
    counts
}

fn render_text(
    left_path: &Path,
    right_path: &Path,
    left_header: &TraceHeader,
    right_header: &TraceHeader,
    left_events: &[NormalizedTraceEvent],
    right_events: &[NormalizedTraceEvent],
    left_counts: &BTreeMap<String, usize>,
    right_counts: &BTreeMap<String, usize>,
    left_route_summary: &RouteSummary,
    right_route_summary: &RouteSummary,
    first_divergence: Option<usize>,
    preview: &[(
        usize,
        Option<NormalizedTraceEvent>,
        Option<NormalizedTraceEvent>,
    )],
) -> String {
    let mut out = String::new();
    out.push_str("DHT Trace Comparison\n");
    out.push_str(&format!("left: {}\n", left_path.display()));
    out.push_str(&format!(
        "  backend: {}\n",
        left_header.backend.as_deref().unwrap_or("unknown")
    ));
    out.push_str(&format!(
        "  owner: {}\n",
        left_header.owner.as_deref().unwrap_or("none")
    ));
    out.push_str(&format!("  lookup events: {}\n", left_events.len()));
    out.push_str(&format!("right: {}\n", right_path.display()));
    out.push_str(&format!(
        "  backend: {}\n",
        right_header.backend.as_deref().unwrap_or("unknown")
    ));
    out.push_str(&format!(
        "  owner: {}\n",
        right_header.owner.as_deref().unwrap_or("none")
    ));
    out.push_str(&format!("  lookup events: {}\n", right_events.len()));
    out.push('\n');

    out.push_str("Event counts:\n");
    for key in ordered_event_keys(left_counts, right_counts) {
        out.push_str(&format!(
            "  {}: left={} right={}\n",
            key,
            left_counts.get(&key).copied().unwrap_or(0),
            right_counts.get(&key).copied().unwrap_or(0),
        ));
    }
    out.push('\n');

    out.push_str("Route updates:\n");
    out.push_str(&format!(
        "  left: total={} inserted_or_added={} retained_or_rejected={} secure={}\n",
        left_route_summary.total,
        left_route_summary.inserted_or_added,
        left_route_summary.retained_or_rejected,
        left_route_summary.secure,
    ));
    out.push_str(&format!(
        "  right: total={} inserted_or_added={} retained_or_rejected={} secure={}\n",
        right_route_summary.total,
        right_route_summary.inserted_or_added,
        right_route_summary.retained_or_rejected,
        right_route_summary.secure,
    ));
    out.push('\n');

    out.push_str("Sequence divergence:\n");
    if let Some(index) = first_divergence {
        out.push_str(&format!(
            "  first mismatch at paired event index {}\n",
            index
        ));
    } else {
        out.push_str("  no event-kind mismatch; sequences align by event type\n");
    }
    out.push('\n');

    out.push_str("Preview:\n");
    for (index, left, right) in preview {
        out.push_str(&format!("  [{}]\n", index));
        out.push_str(&format!(
            "    left:  {}\n",
            render_event_option(left.as_ref())
        ));
        out.push_str(&format!(
            "    right: {}\n",
            render_event_option(right.as_ref())
        ));
    }

    out
}

fn render_event_option(event: Option<&NormalizedTraceEvent>) -> String {
    match event {
        Some(event) => format!(
            "{} target={} summary={} (line {})",
            event.kind,
            event.target.as_deref().unwrap_or("n/a"),
            event.summary,
            event.line_number,
        ),
        None => "none".to_string(),
    }
}

fn route_summary_json(summary: &RouteSummary) -> Value {
    json!({
        "total": summary.total,
        "inserted_or_added": summary.inserted_or_added,
        "retained_or_rejected": summary.retained_or_rejected,
        "secure": summary.secure,
        "kinds": summary.kinds,
    })
}

fn event_json(event: &NormalizedTraceEvent) -> Value {
    json!({
        "backend": event.backend,
        "line_number": event.line_number,
        "kind": event.kind,
        "target": event.target,
        "summary": event.summary,
        "raw": event.raw,
    })
}

fn ordered_event_keys(
    left: &BTreeMap<String, usize>,
    right: &BTreeMap<String, usize>,
) -> Vec<String> {
    let mut keys = left.keys().chain(right.keys()).cloned().collect::<Vec<_>>();
    keys.sort();
    keys.dedup();
    keys
}

fn owner_matches(line: &ParsedTraceLine, owner_filter: Option<&str>) -> bool {
    if line.backend != "mainline" {
        return true;
    }
    match owner_filter {
        Some(owner) if owner != "none" => line
            .fields
            .get("owner")
            .map(|value| value == owner)
            .unwrap_or(false),
        _ => true,
    }
}

fn field<'a>(line: &'a ParsedTraceLine, key: &str) -> &'a str {
    line.fields.get(key).map(String::as_str).unwrap_or("n/a")
}

fn field_equals(line: &ParsedTraceLine, key: &str, expected: &str) -> bool {
    line.fields.get(key).map(String::as_str) == Some(expected)
}

fn field_contains(line: &ParsedTraceLine, key: &str, needle: &str) -> bool {
    line.fields
        .get(key)
        .map(|value| value.contains(needle))
        .unwrap_or(false)
}

fn normalize_target(raw: &str) -> String {
    raw.strip_prefix("Id(")
        .and_then(|value| value.strip_suffix(')'))
        .unwrap_or(raw)
        .to_string()
}

fn parse_key_values(raw: &str) -> BTreeMap<String, String> {
    let bytes = raw.as_bytes();
    let mut markers = Vec::new();
    let mut index = 0;

    while index < bytes.len() {
        let is_start = index == 0 || bytes[index - 1] == b' ';
        if !is_start {
            index += 1;
            continue;
        }

        let mut cursor = index;
        while cursor < bytes.len() && is_key_char(bytes[cursor]) {
            cursor += 1;
        }
        if cursor > index && cursor < bytes.len() && bytes[cursor] == b'=' {
            markers.push((index, cursor));
            index = cursor + 1;
            continue;
        }
        index += 1;
    }

    let mut values = BTreeMap::new();
    for (position, (start, equals)) in markers.iter().enumerate() {
        let key = &raw[*start..*equals];
        let value_start = *equals + 1;
        let value_end = markers
            .get(position + 1)
            .map(|(next_start, _)| next_start.saturating_sub(1))
            .unwrap_or(raw.len());
        let value = raw[value_start..value_end].trim();
        values.insert(key.to_string(), value.to_string());
    }
    values
}

fn is_key_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_key_values_keeps_values_with_spaces() {
        let fields = parse_key_values(
            "event=query_seeded owner=127.0.0.1:6881 request=GetPeers(GetPeersRequestArguments { info_hash: Id(aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa) }) routing_table_size=12",
        );
        assert_eq!(
            fields.get("event").map(String::as_str),
            Some("query_seeded")
        );
        assert_eq!(
            fields.get("owner").map(String::as_str),
            Some("127.0.0.1:6881")
        );
        assert!(fields
            .get("request")
            .is_some_and(|value| value.contains("GetPeersRequestArguments")));
        assert_eq!(
            fields.get("routing_table_size").map(String::as_str),
            Some("12")
        );
    }

    #[test]
    fn normalize_target_strips_mainline_wrapper() {
        assert_eq!(
            normalize_target("Id(aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa)"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(
            normalize_target("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        );
    }
}
