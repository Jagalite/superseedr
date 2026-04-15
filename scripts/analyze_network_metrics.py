#!/usr/bin/env python3
# TEMP-BENCHMARK-ONLY: remove this temporary analyzer before pushing.
import argparse
import collections
import datetime as dt
import json
import math
from pathlib import Path


def parse_args():
    parser = argparse.ArgumentParser(description="Summarize superseedr network metrics JSONL.")
    parser.add_argument("path", help="Path to the JSONL metrics file")
    parser.add_argument("--info-hash", help="Optional lowercase hex info-hash filter")
    parser.add_argument("--from-ts", help="Optional ISO-8601 lower bound")
    parser.add_argument("--to-ts", help="Optional ISO-8601 upper bound")
    return parser.parse_args()


def parse_ts(value):
    if not value:
        return None
    return dt.datetime.fromisoformat(value)


def percentile(values, pct):
    if not values:
        return None
    ordered = sorted(values)
    index = max(0, math.ceil((pct / 100.0) * len(ordered)) - 1)
    return ordered[index]


def mean(values):
    if not values:
        return None
    return sum(values) / len(values)


def format_ms(value):
    if value is None:
        return "n/a"
    return f"{value:.2f}ms"


def source_label_sort_key(item):
    return item[0]


def main():
    args = parse_args()
    metrics_path = Path(args.path)
    from_ts = parse_ts(args.from_ts)
    to_ts = parse_ts(args.to_ts)
    info_hash_filter = args.info_hash.lower() if args.info_hash else None

    total_events = 0
    session_ids = set()
    first_ts = None
    last_ts = None

    candidates_by_source = collections.Counter()
    unique_candidates_by_source = collections.defaultdict(set)
    discovery_mix = collections.defaultdict(lambda: collections.Counter())

    attempts_by_source = collections.Counter()
    successes_by_source = collections.Counter()
    permit_waits_ms = []
    tcp_connect_ms = []

    tracker_started = collections.Counter()
    tracker_completed = collections.Counter()
    tracker_failed = collections.Counter()

    failure_reasons = collections.Counter()
    dropped_events = 0

    inbound_accepted = 0
    inbound_routed = 0
    port_open_marked = 0

    dht_lookups = {}
    internal_family_summaries = collections.defaultdict(list)
    internal_family_end_reasons = collections.defaultdict(collections.Counter)

    with metrics_path.open("r", encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            event = json.loads(line)
            event_ts = parse_ts(event.get("ts"))
            if from_ts and event_ts and event_ts < from_ts:
                continue
            if to_ts and event_ts and event_ts > to_ts:
                continue
            if info_hash_filter and event.get("info_hash") != info_hash_filter:
                continue

            total_events += 1
            session_ids.add(event.get("session_id"))
            if event_ts is not None:
                first_ts = event_ts if first_ts is None else min(first_ts, event_ts)
                last_ts = event_ts if last_ts is None else max(last_ts, event_ts)

            event_type = event.get("event_type")
            source = event.get("source")
            peer_addr = event.get("peer_addr")
            address_family = event.get("address_family")
            fields = event.get("fields") or {}

            if event_type == "peer_candidate_discovered":
                candidates_by_source[source] += 1
                if peer_addr:
                    unique_candidates_by_source[source].add(peer_addr)
                if address_family:
                    discovery_mix[source][address_family] += 1
            elif event_type == "outgoing_connect_requested":
                attempts_by_source[source] += 1
            elif event_type == "outgoing_tcp_connect_succeeded":
                successes_by_source[source] += 1
                if isinstance(fields.get("elapsed_ms"), (int, float)):
                    tcp_connect_ms.append(fields["elapsed_ms"])
            elif event_type == "outgoing_tcp_connect_failed":
                if isinstance(fields.get("elapsed_ms"), (int, float)):
                    tcp_connect_ms.append(fields["elapsed_ms"])
                reason = fields.get("reason")
                if reason:
                    failure_reasons[reason] += 1
            elif event_type == "outgoing_permit_wait":
                if isinstance(fields.get("elapsed_ms"), (int, float)):
                    permit_waits_ms.append(fields["elapsed_ms"])
            elif event_type == "peer_backoff_applied":
                failure_reasons["peer_backoff_applied"] += 1
            elif event_type == "peer_session_ended":
                reason = fields.get("reason")
                if reason:
                    failure_reasons[reason] += 1
            elif event_type == "tracker_announce_started":
                tracker_started[fields.get("scheme", "unknown")] += 1
            elif event_type == "tracker_announce_completed":
                tracker_completed[fields.get("scheme", "unknown")] += 1
            elif event_type == "tracker_announce_failed":
                scheme = fields.get("scheme", "unknown")
                tracker_failed[scheme] += 1
                reason = fields.get("failure_category")
                if reason:
                    failure_reasons[reason] += 1
            elif event_type == "instrumentation_dropped":
                dropped_events = max(dropped_events, fields.get("dropped_events_total", 0))
            elif event_type == "inbound_tcp_accepted":
                inbound_accepted += 1
            elif event_type == "inbound_peer_routed":
                inbound_routed += 1
            elif event_type == "port_open_marked":
                port_open_marked += 1

            if event_type.startswith("dht_lookup_"):
                lookup_id = fields.get("lookup_id")
                if lookup_id:
                    lookup = dht_lookups.setdefault(
                        lookup_id,
                        {"first_batch_ms": None, "peers": 0, "started": False},
                    )
                    if event_type == "dht_lookup_started":
                        lookup["started"] = True
                    elif event_type == "dht_lookup_batch":
                        elapsed = fields.get("elapsed_ms")
                        if lookup["first_batch_ms"] is None and isinstance(elapsed, (int, float)):
                            lookup["first_batch_ms"] = elapsed
                        lookup["peers"] += int(fields.get("batch_size", 0))
            elif event_type == "dht_internal_family_summary":
                key = (
                    fields.get("purpose", "unknown"),
                    fields.get("family", "unknown"),
                )
                internal_family_summaries[key].append(fields)
                internal_family_end_reasons[key][fields.get("ended_reason", "unknown")] += 1

    dht_first_batch_ms = [
        lookup["first_batch_ms"]
        for lookup in dht_lookups.values()
        if lookup["first_batch_ms"] is not None
    ]
    dht_peers_per_lookup = [lookup["peers"] for lookup in dht_lookups.values() if lookup["started"]]

    print("Network Metrics Summary")
    print(f"events: {total_events}")
    print()

    if session_ids:
        print("Session summary:")
        print(f"  unique_sessions: {len(session_ids)}")
        if len(session_ids) == 1:
            print(f"  session_id: {next(iter(session_ids))}")
        if first_ts and last_ts:
            duration = (last_ts - first_ts).total_seconds()
            print(f"  start: {first_ts.isoformat()}")
            print(f"  end: {last_ts.isoformat()}")
            print(f"  duration_seconds: {duration:.3f}")
        print()

    print("Candidates by source:")
    for source, total in sorted(candidates_by_source.items(), key=source_label_sort_key):
        print(
            f"  {source}: total={total} unique={len(unique_candidates_by_source[source])}"
        )
    print()

    print("Outgoing connections by source:")
    all_sources = sorted(set(attempts_by_source) | set(successes_by_source))
    for source in all_sources:
        attempts = attempts_by_source[source]
        successes = successes_by_source[source]
        success_rate = (successes / attempts * 100.0) if attempts else 0.0
        print(
            f"  {source}: attempts={attempts} successes={successes} success_rate={success_rate:.2f}%"
        )
    print()

    print(
        f"Outgoing permit wait: avg={format_ms(mean(permit_waits_ms))} p95={format_ms(percentile(permit_waits_ms, 95))}"
    )
    print(
        f"TCP connect: avg={format_ms(mean(tcp_connect_ms))} p95={format_ms(percentile(tcp_connect_ms, 95))}"
    )
    print()

    print(f"DHT lookups: count={len(dht_lookups)}")
    print(
        f"DHT time to first batch: avg={format_ms(mean(dht_first_batch_ms))} p95={format_ms(percentile(dht_first_batch_ms, 95))}"
    )
    dht_peers_avg = mean(dht_peers_per_lookup)
    dht_peers_p95 = percentile(dht_peers_per_lookup, 95)
    print(
        f"DHT peers per lookup: avg={dht_peers_avg:.2f} p95={dht_peers_p95:.2f}"
        if dht_peers_avg is not None and dht_peers_p95 is not None
        else "DHT peers per lookup: avg=n/a p95=n/a"
    )
    print()

    if internal_family_summaries:
        print("Internal DHT family summaries:")
        for (purpose, family), summaries in sorted(internal_family_summaries.items()):
            def values(field):
                return [
                    entry[field]
                    for entry in summaries
                    if isinstance(entry.get(field), (int, float))
                ]

            visit_cap_hits = sum(
                1 for entry in summaries if entry.get("visit_cap_reached") is True
            )
            peer_cap_hits = sum(
                1 for entry in summaries if entry.get("peer_cap_reached") is True
            )
            print(
                "  "
                f"{purpose}/{family}: "
                f"count={len(summaries)} "
                f"active_routes_available_avg={mean(values('active_routes_available')):.2f} "
                f"cached_routes_available_avg={mean(values('cached_routes_available')):.2f} "
                f"seeded_total_avg={mean(values('seeded_total')):.2f} "
                f"seeded_bootstrap_avg={mean(values('seeded_bootstrap')):.2f} "
                f"seeded_cached_avg={mean(values('seeded_cached')):.2f} "
                f"initial_wave_avg={mean(values('initial_wave_limit')):.2f} "
                f"visited_avg={mean(values('visited')):.2f} "
                f"query_success_avg={mean(values('query_successes')):.2f} "
                f"query_failure_avg={mean(values('query_failures')):.2f} "
                f"peer_values_seen_avg={mean(values('peer_values_seen')):.2f} "
                f"peer_values_before_first_batch_avg={mean(values('peer_values_before_first_batch')):.2f} "
                f"unique_peers_before_first_batch_avg={mean(values('unique_peers_before_first_batch')):.2f} "
                f"duplicate_peers_before_first_batch_avg={mean(values('duplicate_peers_before_first_batch')):.2f} "
                f"peer_values_after_first_batch_avg={mean(values('peer_values_after_first_batch')):.2f} "
                f"unique_peers_after_first_batch_avg={mean(values('unique_peers_after_first_batch')):.2f} "
                f"duplicate_peers_after_first_batch_avg={mean(values('duplicate_peers_after_first_batch')):.2f} "
                f"responses_with_peers_avg={mean(values('responses_with_peers')):.2f} "
                f"responses_with_peers_before_first_batch_avg={mean(values('responses_with_peers_before_first_batch')):.2f} "
                f"responses_with_peers_after_first_batch_avg={mean(values('responses_with_peers_after_first_batch')):.2f} "
                f"nodes_discovered_avg={mean(values('nodes_discovered')):.2f} "
                f"nodes_accepted_avg={mean(values('nodes_accepted')):.2f} "
                f"nodes_rejected_avg={mean(values('nodes_rejected')):.2f} "
                f"peers_avg={mean(values('peers')):.2f} "
                f"max_pending_p95={percentile(values('max_pending'), 95):.2f} "
                f"visit_cap_hits={visit_cap_hits} "
                f"peer_cap_hits={peer_cap_hits}"
            )
            reasons = ", ".join(
                f"{reason}={count}"
                for reason, count in internal_family_end_reasons[(purpose, family)].most_common()
            )
            print(f"    end_reasons: {reasons}")
        print()

    print("Discovery mix by source:")
    for source, families in sorted(discovery_mix.items(), key=lambda item: item[0]):
        print(
            f"  {source}: ipv4={families.get('ipv4', 0)} ipv6={families.get('ipv6', 0)}"
        )
    print()

    print("Tracker announces:")
    for scheme in sorted(set(tracker_started) | set(tracker_completed) | set(tracker_failed)):
        print(
            f"  {scheme}: started={tracker_started[scheme]} completed={tracker_completed[scheme]} failed={tracker_failed[scheme]}"
        )
    print()

    print("Top failure reasons:")
    for reason, count in failure_reasons.most_common(10):
        print(f"  {reason}: {count}")
    print()

    print(f"Dropped instrumentation events: {dropped_events}")
    print()

    tracker_sources = ["tracker_http", "tracker_udp", "tracker_other"]
    tracker_candidates = sum(candidates_by_source[source] for source in tracker_sources)
    tracker_attempts = sum(attempts_by_source[source] for source in tracker_sources)
    tracker_successes = sum(successes_by_source[source] for source in tracker_sources)
    tracker_success_rate = (
        tracker_successes / tracker_attempts * 100.0 if tracker_attempts else 0.0
    )
    dht_attempts = attempts_by_source["dht"]
    dht_successes = successes_by_source["dht"]
    dht_success_rate = dht_successes / dht_attempts * 100.0 if dht_attempts else 0.0

    print("DHT vs tracker:")
    print(
        f"  dht: candidates={candidates_by_source['dht']} attempts={dht_attempts} successes={dht_successes} success_rate={dht_success_rate:.2f}%"
    )
    print(
        f"  tracker: candidates={tracker_candidates} attempts={tracker_attempts} successes={tracker_successes} success_rate={tracker_success_rate:.2f}%"
    )
    print()

    print("Inbound summary:")
    print(f"  accepted={inbound_accepted}")
    print(f"  routed={inbound_routed}")
    print(f"  port_open_marked={port_open_marked}")


if __name__ == "__main__":
    main()
