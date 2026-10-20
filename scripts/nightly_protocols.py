#!/usr/bin/env python3
"""Bounded sanitizer campaigns for every target registered in fuzz/Cargo.toml."""
import argparse
import json
import os
import signal
from pathlib import Path
import subprocess
import time
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seconds", type=int, default=60)
    args = parser.parse_args()
    if args.seconds < 1:
        parser.error("--seconds must be positive")
    targets = [entry["name"] for entry in tomllib.loads((ROOT / "fuzz/Cargo.toml").read_text())["bin"]]
    assert targets and len(set(targets)) == len(targets)
    subprocess.run(["python3", "scripts/fuzz_seed_corpus.py"], cwd=ROOT, check=True)
    output = ROOT / "target/nightly-results"
    output.mkdir(parents=True, exist_ok=True)
    receipts = []
    for target in targets:
        command = ["cargo", "+nightly", "fuzz", "run", target, "--",
                   f"-max_total_time={args.seconds}", "-timeout=10", "-rss_limit_mb=2048",
                   "-max_len=65536", "-print_final_stats=1"]
        dictionary = ROOT / f"fuzz/dictionaries/{target}.dict"
        if dictionary.exists():
            command.append(f"-dict={dictionary}")
        receipt = {"target": target, "command": command, "status": "running"}
        receipts.append(receipt)
        report = output / "protocols.json"
        report.write_text(json.dumps(receipts, indent=2) + "\n")
        print(f"Running {target} for {args.seconds}s", flush=True)
        started = time.monotonic()
        with (output / f"{target}.log").open("w") as log:
            process = subprocess.Popen(command, cwd=ROOT, stdout=log,
                                       stderr=subprocess.STDOUT, start_new_session=True)
            try:
                returncode = process.wait(timeout=args.seconds + 120)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
                returncode = 124
        receipt.update(returncode=returncode,
                       status="passed" if returncode == 0 else "failed",
                       elapsed_seconds=round(time.monotonic() - started, 3))
        report.write_text(json.dumps(receipts, indent=2) + "\n")
        print((output / f"{target}.log").read_text(errors="replace"), flush=True)
    raise SystemExit(0 if all(r["status"] == "passed" for r in receipts) else 1)


if __name__ == "__main__":
    main()
