#!/usr/bin/env python3
"""Run an explicit property campaign against a prebuilt libtest executable."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import signal
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
STATE = "torrent_manager::state::prop_tests::"
ASSIGN = "torrent_manager::state::assignment_tests::"
# Exact names make renames/removals fail instead of silently running zero tests.
CAMPAIGNS = {
    "baseline": (256, []),
    "state": (1_000_000, [STATE + name for name in (
        "fuzz_piece_block_selection_and_completion", "test_stateful_stories",
        "test_deep_state_chaos", "test_tit_for_tat_fairness", "test_rarest_first_selection",
        "test_tit_for_tat_snubbed", "test_rarest_first_tie", "test_choke_during_pick",
        "test_free_rider_justice", "test_choke_race_condition",
        "state_machine::test_lifecycle_state_machine",
        "state_machine::test_state_machine_network_faults",
        "state_machine::test_state_machine_network_reordering",
    )]),
    "state-scale": (1_000_000, [STATE + "test_rarest_first_scale"]),
    "dht": (1_000_000, [
        "dht::lookup::tests::lookup_state_random_walk_fuzz_preserves_core_invariants",
        "dht::scheduler::tests::demand_scheduler_state_fuzz_keeps_entries_consistent",
        "dht::service::planner::reducer_tests::demand_planner_state_machine_fuzz_preserves_capacity_and_entry_invariants",
        "dht::service::planner::selection_tests::demand_planner_selection_fuzz_respects_caps_budget_and_stats",
    ]),
    "storage": (1_000_000, [
        "torrent_manager::block_bitfield::tests::packed_bits_match_byte_flags",
        "torrent_manager::block_bitfield::tests::updates_and_resets_match_after_every_operation",
        "torrent_manager::block_manager::tests::prop_rarity_rebuild_matches_original_across_snapshots",
    ]),
    "telemetry": (1_000_000, [
        "telemetry::auto_graph::tests::burst_grouping_is_stable_across_rates_gaps_and_small_noise",
        "telemetry::manager_telemetry::tests::publication_matches_clone_based_reference",
    ]),
    "assignment": (1_000_000, [ASSIGN + "prop_full_refill_preserves_work_without_scanning",
                               ASSIGN + "prop_assignment_matches_original_stable_sort"]),
    "assignment-scale": (2_000, [ASSIGN + "prop_full_refill_large", ASSIGN + "prop_assignment_large"]),
}


def positive(value):
    value = int(value)
    if value <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return value


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("campaign", choices=CAMPAIGNS)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--cases", type=positive, help="local smoke override, recorded in receipt")
    parser.add_argument("--timeout", type=positive, default=2100)
    parser.add_argument("--output", type=Path, default=ROOT / "target/nightly-results")
    args = parser.parse_args()
    cases, names = CAMPAIGNS[args.campaign]
    cases = args.cases or cases
    args.output.mkdir(parents=True, exist_ok=True)
    log_path = args.output / f"{args.campaign}.log"
    receipt_path = args.output / f"{args.campaign}.json"
    seed = os.environ.get("PROPTEST_RNG_SEED", str(secrets.randbits(64)))
    env = dict(os.environ, PROPTEST_CASES=str(cases), PROPTEST_RNG_SEED=seed,
               PROPTEST_MAX_SHRINK_TIME="60000")
    command = [str(args.binary.resolve())]
    if names:
        command += ["--exact", *names]
    if args.campaign == "assignment-scale":
        command += ["--ignored"]
    command += ["--test-threads=2"]
    binary_hash = hashlib.sha256(args.binary.read_bytes()).hexdigest()
    receipt = {"binary_sha256": binary_hash, "campaign": args.campaign, "cases_per_property": cases, "seed": seed, "max_shrink_ms": 60000,
               "command": command, "timeout_seconds": args.timeout, "status": "starting",
               "source_dirty": bool(subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=no"], cwd=ROOT)),
               "source_sha": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()}

    def save():
        receipt_path.write_text(json.dumps(receipt, indent=2) + "\n")

    save()
    # Verify discovery before execution. A typo must fail even if libtest exits 0.
    listing = subprocess.run(command + ["--list", "--format=terse"], cwd=ROOT, env=env,
                             text=True, capture_output=True, timeout=60, check=True).stdout
    discovered = {line.removesuffix(": test") for line in listing.splitlines() if line.endswith(": test")}
    if not discovered or (names and discovered != set(names)):
        receipt.update(status="discovery_failed", discovered=sorted(discovered))
        save()
        raise SystemExit(f"unexpected test discovery: {sorted(discovered)}")
    receipt.update(status="running", discovered=sorted(discovered))
    save()
    print(json.dumps(receipt), flush=True)
    started = time.monotonic()
    with log_path.open("w") as log:
        process = subprocess.Popen(command, cwd=ROOT, env=env, stdout=log,
                                   stderr=subprocess.STDOUT, start_new_session=True)
        timed_out = False
        while process.poll() is None:
            elapsed = time.monotonic() - started
            if elapsed >= args.timeout:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
                timed_out = True
                break
            print(f"{args.campaign}: {elapsed:.0f}s elapsed; log={log_path}", flush=True)
            try:
                process.wait(timeout=min(30, args.timeout - elapsed))
            except subprocess.TimeoutExpired:
                pass
    output = log_path.read_text(errors="replace")
    print(output, end="", flush=True)
    passed = re.findall(r"^test (\S+) \.\.\. ok$", output, re.MULTILINE)
    complete = set(passed) == set(names) if names else bool(passed)
    status = "timeout" if timed_out else ("passed" if process.returncode == 0 and complete else "failed")
    receipt.update(status=status, returncode=process.returncode,
                   elapsed_seconds=round(time.monotonic() - started, 3), passed=passed,
                   incomplete=sorted(set(names) - set(passed)))
    save()
    raise SystemExit(0 if status == "passed" else 1)


if __name__ == "__main__":
    main()
