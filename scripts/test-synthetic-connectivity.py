#!/usr/bin/env python3
"""Exercise real synthetic connectivity workloads; this is acceptance, not a speed benchmark."""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/superseedr"))
    parser.add_argument("--case", help="Run one named case")
    parser.add_argument("--out", type=Path, help="Retain summaries here; generated payloads are temporary")
    args = parser.parse_args()
    binary = args.binary.resolve()
    cases = []
    for transport in ("tcp", "utp", "webrtc", "mixed"):
        cases.append((f"{transport}-idle", ["--transport", transport, "--activity", "idle", "--mode", "swarm", "--peers", "6" if transport == "mixed" else "4"]))
    for transport in ("tcp", "utp", "webrtc"):
        cases.append((f"{transport}-churn", ["--transport", transport, "--activity", "idle", "--mode", "upload", "--peers", "2", "--session-lifetime-ms", "1000"]))
    for transport in ("tcp", "utp", "webrtc"):
        cases.append((f"{transport}-download-churn", ["--transport", transport, "--activity", "idle", "--mode", "download", "--peers", "2", "--session-lifetime-ms", "1000"]))
    cases.append(("webrtc-download-payload", ["--transport", "webrtc", "--activity", "payload", "--mode", "download", "--peers", "2"]))
    cases.append(("webrtc-long-idle", ["--transport", "webrtc", "--activity", "idle", "--mode", "swarm", "--peers", "4"]))
    cases.append(("mixed-payload", ["--transport", "mixed", "--activity", "mixed", "--mode", "upload", "--peers", "12"]))
    for transport in ("tcp", "webrtc"):
        for failure in ("reject-handshake", "stall-handshake"):
            cases.append((f"{transport}-{failure}", ["--transport", transport, "--activity", "idle", "--mode", "upload", "--peers", "2", "--failure-percent", "100", "--failure-case", failure, "--handshake-timeout-ms", "2000"]))
    if args.case:
        cases = [case for case in cases if case[0] == args.case]
        if not cases:
            parser.error("unknown case")
    if args.out:
        args.out.mkdir(parents=True, exist_ok=True)
    for name, options in cases:
        with tempfile.TemporaryDirectory(prefix="superseedr-synthetic-") as temp:
            command = [str(binary), "--json", "synthetic-load", "--torrents", "1", "--duration-secs", "40" if name == "webrtc-long-idle" else "8", "--warmup-secs", "1", "--metrics-interval-ms", "500", "--keepalive-ms", "200", "--tracker-interval-secs", "1", "--size-per-torrent", "64KiB", "--piece-size", "16KiB", "--out", temp, *options]
            result = subprocess.run(command, text=True, capture_output=True, timeout=70)
            reports = list(Path(temp).glob("*/summary.json"))
            if not reports:
                raise AssertionError(f"{name}: no summary\n{result.stdout}\n{result.stderr}")
            summary = json.loads(reports[0].read_text())
            if args.out:
                (args.out / f"{name}.json").write_text(json.dumps(summary, indent=2) + "\n")
            assert result.returncode == 0, f"{name}: {result.stderr}\n{summary.get('session_issues')}"
            assert summary["session_issues"] == [], (name, summary["session_issues"])
            sessions = summary["sessions"]
            assert summary["sessions_after_shutdown"]["active"] == 0, name
            assert sessions["idle_payload_bytes"] == 0, name
            if "handshake" in name:
                assert sessions["expected_failures"] >= 2, (name, sessions)
                assert sessions["established"] == 0, (name, sessions)
            elif name.endswith("churn"):
                assert sessions["planned_disconnects"] >= 2, (name, sessions)
                assert sessions["established"] > 2, (name, sessions)
            elif name.endswith("idle"):
                assert sessions["active"] == summary["requested_peers"], (name, sessions)
                assert sessions["keepalives_sent"] > 0, (name, sessions)
            elif name == "webrtc-download-payload":
                # The small download may finish during warmup; block counters cover the whole run.
                assert summary["manager_block_received"] > 0, name
                assert summary["completed_pieces"] == summary["total_pieces"], name
            elif name == "mixed-payload":
                assert summary["upload_bytes"] > 0, name
                assert sessions["keepalives_sent"] > 0, name
            if not name.endswith("payload"):
                assert summary["download_bytes"] == summary["upload_bytes"] == 0, name
                assert summary["manager_block_received"] == summary["manager_block_sent"] == 0, name
                assert summary["disk_read_started"] == summary["disk_write_started"] == 0, name
            if "webrtc" in name or "mixed" in name:
                assert sessions["rtc_connected"] > 0 and sessions["tracker_answers"] > 0, (name, sessions)
            print(f"PASS {name}: {sessions['established']} handshakes, {sessions['planned_disconnects']} planned closes", flush=True)


if __name__ == "__main__":
    main()
