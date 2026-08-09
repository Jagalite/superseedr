#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025 The superseedr Contributors
# SPDX-License-Identifier: GPL-3.0-or-later

"""Turn strace network-syscall output into a stable socket profile manifest."""

from __future__ import annotations

import argparse
import collections
import ipaddress
import json
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Iterable


SYSCALL_RE = re.compile(
    r"^(?P<timestamp>\d+\.\d+)\s+"
    r"(?P<name>[a-zA-Z0-9_]+)\((?P<args>.*)\)\s+=\s+(?P<result>.+)$"
)
IPV4_RE = re.compile(r'inet_addr\("([^"]+)"\)')
IPV6_RE = re.compile(r'inet_pton\(AF_INET6, "([^"]+)"[^)]*\)')
PORT_RE = re.compile(r"(sin6?_port=htons\()\d+(\))")
NETLINK_PID_RE = re.compile(r"(nl_pid=)\d+")


@dataclass
class SocketRecord:
    creator: str
    operations: list[str] = field(default_factory=list)

    def profile(self) -> dict[str, object]:
        return {
            "creator": self.creator,
            "operations": sorted(set(self.operations)),
        }


def split_top_level(value: str) -> list[str]:
    parts: list[str] = []
    start = 0
    depth = 0
    quote: str | None = None
    escaped = False
    for index, char in enumerate(value):
        if quote is not None:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == quote:
                quote = None
            continue
        if char in {'"', "'"}:
            quote = char
        elif char in "([{":
            depth += 1
        elif char in ")]}" and depth > 0:
            depth -= 1
        elif char == "," and depth == 0:
            parts.append(value[start:index].strip())
            start = index + 1
    parts.append(value[start:].strip())
    return parts


def address_class(value: str) -> str:
    try:
        address = ipaddress.ip_address(value)
    except ValueError:
        return "<ADDRESS>"
    if address.is_unspecified:
        return "<ANY>"
    if address.is_loopback:
        return "<LOOPBACK>"
    if address.is_link_local:
        return "<LINK_LOCAL>"
    if address.is_private:
        return "<PRIVATE>"
    return "<REMOTE>"


def normalize_dynamic(value: str) -> str:
    value = PORT_RE.sub(lambda match: f"{match.group(1)}<PORT>{match.group(2)}", value)
    value = IPV4_RE.sub(lambda match: f'inet_addr("{address_class(match.group(1))}")', value)
    value = IPV6_RE.sub(
        lambda match: f'inet_pton(AF_INET6, "{address_class(match.group(1))}")', value
    )
    value = NETLINK_PID_RE.sub(r"\1<PID>", value)
    value = value.replace("/state/main/", "/state/<REVISION>/")
    value = value.replace("/state/branch/", "/state/<REVISION>/")
    return re.sub(r"\s+", " ", value).strip()


def parse_fd(value: str) -> int | None:
    match = re.match(r"(-?\d+)", value)
    return int(match.group(1)) if match else None


def successful_fd(result: str) -> int | None:
    fd = parse_fd(result)
    return fd if fd is not None and fd >= 0 else None


def read_events(trace_dir: Path) -> Iterable[tuple[float, str, str, str]]:
    events: list[tuple[float, str, str, str]] = []
    for trace_path in sorted(trace_dir.glob("trace*")):
        if not trace_path.is_file():
            continue
        for line in trace_path.read_text(encoding="utf-8", errors="replace").splitlines():
            match = SYSCALL_RE.match(line)
            if match is None:
                continue
            events.append(
                (
                    float(match.group("timestamp")),
                    match.group("name"),
                    match.group("args"),
                    match.group("result"),
                )
            )
    return sorted(events, key=lambda event: event[0])


def build_manifest(trace_dir: Path) -> dict[str, object]:
    records: list[SocketRecord] = []
    descriptors: dict[int, int] = {}
    failed_socket_calls: collections.Counter[str] = collections.Counter()

    for _, name, raw_args, result in read_events(trace_dir):
        args = split_top_level(raw_args)
        if name == "socket":
            fd = successful_fd(result)
            creator = normalize_dynamic(f"socket({raw_args})")
            if fd is None:
                failed_socket_calls[creator] += 1
                continue
            descriptors[fd] = len(records)
            records.append(SocketRecord(creator=creator))
            continue

        if name in {"accept", "accept4"}:
            fd = successful_fd(result)
            if fd is None:
                continue
            flags = args[3] if name == "accept4" and len(args) > 3 else "0"
            descriptors[fd] = len(records)
            records.append(SocketRecord(creator=f"{name}(flags={normalize_dynamic(flags)})"))
            continue

        if name in {"dup", "dup2", "dup3"}:
            if not args:
                continue
            source_fd = parse_fd(args[0])
            target_fd = successful_fd(result)
            if source_fd in descriptors and target_fd is not None:
                descriptors[target_fd] = descriptors[source_fd]
            continue

        if not args:
            continue
        fd = parse_fd(args[0])
        if fd is None:
            continue
        if name == "close":
            descriptors.pop(fd, None)
            continue
        record_index = descriptors.get(fd)
        if record_index is None:
            continue
        record = records[record_index]

        operation: str | None = None
        if name == "setsockopt" and len(args) >= 4:
            operation = f"setsockopt({', '.join(args[1:])})"
        elif name == "bind" and len(args) >= 2:
            operation = f"bind({', '.join(args[1:])})"
        elif name == "connect" and len(args) >= 2:
            operation = f"connect({', '.join(args[1:])})"
        elif name == "listen" and len(args) >= 2:
            operation = f"listen({args[1]})"
        elif name == "fcntl" and len(args) >= 2 and args[1] in {"F_SETFL", "F_SETFD"}:
            operation = f"fcntl({', '.join(args[1:])})"
        elif name == "ioctl" and len(args) >= 2 and args[1] == "FIONBIO":
            operation = f"ioctl({', '.join(args[1:])})"
        if operation is not None:
            record.operations.append(normalize_dynamic(operation))

    profiles: collections.Counter[str] = collections.Counter()
    decoded_profiles: dict[str, dict[str, object]] = {}
    creators: collections.Counter[str] = collections.Counter()
    incomplete: collections.Counter[str] = collections.Counter()
    for record in records:
        creators[record.creator] += 1
        if not record.operations:
            incomplete[record.creator] += 1
            continue
        profile = record.profile()
        key = json.dumps(profile, sort_keys=True, separators=(",", ":"))
        profiles[key] += 1
        decoded_profiles[key] = profile

    profile_rows = [
        {**decoded_profiles[key], "observed_count": profiles[key]} for key in sorted(profiles)
    ]
    return {
        "schema": 2,
        "comparison_scope": "successful socket/accept creation and explicit static configuration",
        "normalization": {
            "addresses": "classification only",
            "ports": "dynamic/static numeric values removed",
            "ordering": "constructor/profile sets sorted; operation duplicates removed",
            "ignored": "timestamps, process/thread ids, file descriptors, syscall results",
        },
        "observed_socket_count": len(records),
        "observed_incomplete_socket_count": sum(incomplete.values()),
        "constructors": [
            {"creator": creator, "observed_count": count}
            for creator, count in sorted(creators.items())
        ],
        "incomplete_at_shutdown": [
            {"creator": creator, "observed_count": count}
            for creator, count in sorted(incomplete.items())
        ],
        "unique_profile_count": len(profile_rows),
        "profiles": profile_rows,
        "failed_socket_calls": [
            {"creator": creator, "observed_count": count}
            for creator, count in sorted(failed_socket_calls.items())
        ],
    }


def static_profiles(manifest: dict[str, object]) -> set[str]:
    return {
        json.dumps(
            {"creator": row["creator"], "operations": row["operations"]},
            sort_keys=True,
            separators=(",", ":"),
        )
        for row in manifest["profiles"]
    }


def maximal_static_profiles(manifest: dict[str, object]) -> set[str]:
    """Return profiles that are not strict partial observations of another profile.

    A connection can end before later configuration calls run. Those partial
    lifecycles remain in ``profiles`` for diagnosis, but must not create a static
    parity failure when the same constructor's complete operation superset was
    observed in that revision.
    """
    profiles = [
        (row["creator"], frozenset(row["operations"])) for row in manifest["profiles"]
    ]
    maximal: set[str] = set()
    for creator, operations in profiles:
        if any(
            creator == other_creator and operations < other_operations
            for other_creator, other_operations in profiles
        ):
            continue
        maximal.add(
            json.dumps(
                {"creator": creator, "operations": sorted(operations)},
                sort_keys=True,
                separators=(",", ":"),
            )
        )
    return maximal


def static_constructors(manifest: dict[str, object]) -> set[str]:
    return {row["creator"] for row in manifest["constructors"]}


def merge_manifests(paths: list[Path]) -> dict[str, object]:
    if not paths:
        raise ValueError("at least one manifest is required")

    constructors: collections.Counter[str] = collections.Counter()
    incomplete: collections.Counter[str] = collections.Counter()
    failed_socket_calls: collections.Counter[str] = collections.Counter()
    profiles: collections.Counter[str] = collections.Counter()
    decoded_profiles: dict[str, dict[str, object]] = {}
    observed_socket_count = 0
    observed_incomplete_socket_count = 0

    for path in paths:
        manifest = json.loads(path.read_text(encoding="utf-8"))
        if manifest.get("schema") != 2:
            raise ValueError(f"unsupported manifest schema in {path}")
        observed_socket_count += manifest["observed_socket_count"]
        observed_incomplete_socket_count += manifest["observed_incomplete_socket_count"]
        constructors.update(
            {row["creator"]: row["observed_count"] for row in manifest["constructors"]}
        )
        incomplete.update(
            {
                row["creator"]: row["observed_count"]
                for row in manifest["incomplete_at_shutdown"]
            }
        )
        failed_socket_calls.update(
            {
                row["creator"]: row["observed_count"]
                for row in manifest["failed_socket_calls"]
            }
        )
        for row in manifest["profiles"]:
            profile = {"creator": row["creator"], "operations": row["operations"]}
            key = json.dumps(profile, sort_keys=True, separators=(",", ":"))
            profiles[key] += row["observed_count"]
            decoded_profiles[key] = profile

    profile_rows = [
        {**decoded_profiles[key], "observed_count": profiles[key]} for key in sorted(profiles)
    ]
    return {
        "schema": 2,
        "comparison_scope": "union of successful socket construction profiles across fresh repeated runs",
        "merged_run_count": len(paths),
        "normalization": {
            "addresses": "classification only",
            "ports": "dynamic/static numeric values removed",
            "ordering": "constructor/profile sets sorted; operation duplicates removed",
            "ignored": "timestamps, process/thread ids, file descriptors, syscall results",
        },
        "observed_socket_count": observed_socket_count,
        "observed_incomplete_socket_count": observed_incomplete_socket_count,
        "constructors": [
            {"creator": creator, "observed_count": count}
            for creator, count in sorted(constructors.items())
        ],
        "incomplete_at_shutdown": [
            {"creator": creator, "observed_count": count}
            for creator, count in sorted(incomplete.items())
        ],
        "unique_profile_count": len(profile_rows),
        "profiles": profile_rows,
        "failed_socket_calls": [
            {"creator": creator, "observed_count": count}
            for creator, count in sorted(failed_socket_calls.items())
        ],
    }


def compare(main_path: Path, branch_path: Path) -> tuple[dict[str, object], bool]:
    main_manifest = json.loads(main_path.read_text(encoding="utf-8"))
    branch_manifest = json.loads(branch_path.read_text(encoding="utf-8"))
    main_profiles = static_profiles(main_manifest)
    branch_profiles = static_profiles(branch_manifest)
    main_maximal_profiles = maximal_static_profiles(main_manifest)
    branch_maximal_profiles = maximal_static_profiles(branch_manifest)
    main_constructors = static_constructors(main_manifest)
    branch_constructors = static_constructors(branch_manifest)
    only_main = sorted(main_maximal_profiles - branch_maximal_profiles)
    only_branch = sorted(branch_maximal_profiles - main_maximal_profiles)
    only_main_observed = sorted(main_profiles - branch_profiles)
    only_branch_observed = sorted(branch_profiles - main_profiles)
    only_main_constructors = sorted(main_constructors - branch_constructors)
    only_branch_constructors = sorted(branch_constructors - main_constructors)
    matches = (
        not only_main
        and not only_branch
        and not only_main_constructors
        and not only_branch_constructors
    )
    result = {
        "schema": 2,
        "static_profiles_match": matches,
        "observed_profiles_match": not only_main_observed and not only_branch_observed,
        "constructor_sets_match": not only_main_constructors and not only_branch_constructors,
        "main_observed_socket_count": main_manifest["observed_socket_count"],
        "branch_observed_socket_count": branch_manifest["observed_socket_count"],
        "main_incomplete_at_shutdown": main_manifest["observed_incomplete_socket_count"],
        "branch_incomplete_at_shutdown": branch_manifest["observed_incomplete_socket_count"],
        "only_main_constructors": only_main_constructors,
        "only_branch_constructors": only_branch_constructors,
        "only_main": [json.loads(profile) for profile in only_main],
        "only_branch": [json.loads(profile) for profile in only_branch],
        "only_main_observed": [json.loads(profile) for profile in only_main_observed],
        "only_branch_observed": [json.loads(profile) for profile in only_branch_observed],
        "count_differences": [],
    }
    main_counts = {
        json.dumps(
            {"creator": row["creator"], "operations": row["operations"]},
            sort_keys=True,
            separators=(",", ":"),
        ): row["observed_count"]
        for row in main_manifest["profiles"]
    }
    branch_counts = {
        json.dumps(
            {"creator": row["creator"], "operations": row["operations"]},
            sort_keys=True,
            separators=(",", ":"),
        ): row["observed_count"]
        for row in branch_manifest["profiles"]
    }
    result["count_differences"] = [
        {
            "profile": json.loads(profile),
            "main": main_counts.get(profile, 0),
            "branch": branch_counts.get(profile, 0),
        }
        for profile in sorted(main_profiles | branch_profiles)
        if main_counts.get(profile, 0) != branch_counts.get(profile, 0)
    ]
    return result, matches


def main() -> int:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)
    normalize_parser = subparsers.add_parser("normalize")
    normalize_parser.add_argument("trace_dir", type=Path)
    normalize_parser.add_argument("output", type=Path)
    merge_parser = subparsers.add_parser("merge")
    merge_parser.add_argument("output", type=Path)
    merge_parser.add_argument("manifests", nargs="+", type=Path)
    compare_parser = subparsers.add_parser("compare")
    compare_parser.add_argument("main_manifest", type=Path)
    compare_parser.add_argument("branch_manifest", type=Path)
    compare_parser.add_argument("output", type=Path)
    args = parser.parse_args()

    if args.command == "normalize":
        result = build_manifest(args.trace_dir)
        matches = True
    elif args.command == "merge":
        result = merge_manifests(args.manifests)
        matches = True
    else:
        result, matches = compare(args.main_manifest, args.branch_manifest)
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return 0 if matches else 1


if __name__ == "__main__":
    raise SystemExit(main())
