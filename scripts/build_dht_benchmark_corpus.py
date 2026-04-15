#!/usr/bin/env python3

import argparse
import json
import pathlib
import sys


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build a DHT benchmark corpus from network metrics JSONL."
    )
    parser.add_argument(
        "input",
        nargs="?",
        default="tmp/network_metrics.jsonl",
        help="Input network metrics JSONL file.",
    )
    parser.add_argument(
        "output",
        nargs="?",
        default="tmp/dht_benchmark_infohashes.txt",
        help="Output corpus file path.",
    )
    parser.add_argument(
        "--limit",
        type=int,
        default=None,
        help="Optional maximum number of unique info hashes to write.",
    )
    return parser.parse_args()


def maybe_collect_info_hashes(record: dict) -> list[str]:
    values: list[str] = []
    info_hash = record.get("info_hash")
    if isinstance(info_hash, str):
        values.append(info_hash)
    fields = record.get("fields")
    if isinstance(fields, dict):
        nested = fields.get("info_hash")
        if isinstance(nested, str):
            values.append(nested)
    return values


def is_valid_info_hash(value: str) -> bool:
    if len(value) != 40:
        return False
    return all(char in "0123456789abcdefABCDEF" for char in value)


def main() -> int:
    args = parse_args()
    input_path = pathlib.Path(args.input)
    output_path = pathlib.Path(args.output)

    if not input_path.exists():
        print(f"Input file not found: {input_path}", file=sys.stderr)
        return 1

    info_hashes: set[str] = set()
    with input_path.open("r", encoding="utf-8") as handle:
        for line_number, line in enumerate(handle, start=1):
            line = line.strip()
            if not line:
                continue
            try:
                record = json.loads(line)
            except json.JSONDecodeError as error:
                print(
                    f"Skipping malformed JSON on line {line_number}: {error}",
                    file=sys.stderr,
                )
                continue
            if not isinstance(record, dict):
                continue
            for value in maybe_collect_info_hashes(record):
                if is_valid_info_hash(value):
                    info_hashes.add(value.lower())
                    if args.limit is not None and len(info_hashes) >= args.limit:
                        break
            if args.limit is not None and len(info_hashes) >= args.limit:
                break

    if not info_hashes:
        print(
            f"No info hashes found in {input_path}.",
            file=sys.stderr,
        )
        return 1

    output_path.parent.mkdir(parents=True, exist_ok=True)
    with output_path.open("w", encoding="utf-8", newline="\n") as handle:
        for info_hash in sorted(info_hashes):
            handle.write(info_hash)
            handle.write("\n")

    print(f"Wrote {len(info_hashes)} info hashes to {output_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
