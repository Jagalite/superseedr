#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2025 The superseedr Contributors
# SPDX-License-Identifier: GPL-3.0-or-later

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from normalize_strace import build_manifest, compare, merge_manifests


CONNECTED_SOCKET = """\
1.000001 socket(AF_INET, SOCK_STREAM|SOCK_CLOEXEC|SOCK_NONBLOCK, IPPROTO_IP) = 7
1.000002 connect(7, {sa_family=AF_INET, sin_port=htons(41000), sin_addr=inet_addr("198.51.100.10")}, 16) = -1 EINPROGRESS (Operation now in progress)
"""


class NormalizeStraceTests(unittest.TestCase):
    def write_trace(self, root: Path, contents: str) -> None:
        root.mkdir(parents=True)
        (root / "trace.1").write_text(contents, encoding="utf-8")

    def write_manifest(self, root: Path, contents: str) -> Path:
        trace_dir = root / "raw"
        self.write_trace(trace_dir, contents)
        manifest_path = root / "manifest.json"
        manifest_path.write_text(
            json.dumps(build_manifest(trace_dir)), encoding="utf-8"
        )
        return manifest_path

    def test_shutdown_incomplete_socket_does_not_change_static_parity(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            main_path = self.write_manifest(
                root / "main",
                CONNECTED_SOCKET
                + "2.000001 socket(AF_INET, SOCK_STREAM|SOCK_CLOEXEC|SOCK_NONBLOCK, IPPROTO_IP) = 8\n",
            )
            branch_path = self.write_manifest(root / "branch", CONNECTED_SOCKET)
            comparison, matches = compare(main_path, branch_path)

        self.assertTrue(matches)
        self.assertTrue(comparison["constructor_sets_match"])
        self.assertEqual(comparison["main_incomplete_at_shutdown"], 1)
        self.assertEqual(comparison["branch_incomplete_at_shutdown"], 0)

    def test_explicit_option_difference_fails_static_parity(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            main_path = self.write_manifest(
                root / "main",
                CONNECTED_SOCKET
                + "1.000003 setsockopt(7, SOL_SOCKET, SO_KEEPALIVE, [1], 4) = 0\n",
            )
            branch_path = self.write_manifest(root / "branch", CONNECTED_SOCKET)
            comparison, matches = compare(main_path, branch_path)

        self.assertFalse(matches)
        self.assertEqual(len(comparison["only_main"]), 1)
        self.assertEqual(len(comparison["only_branch"]), 1)

    def test_partial_lifecycle_is_diagnostic_when_complete_profile_matches(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            complete = (
                CONNECTED_SOCKET
                + "1.000003 setsockopt(7, SOL_TCP, TCP_NODELAY, [1], 4) = 0\n"
            )
            main_path = self.write_manifest(
                root / "main",
                complete
                + "2.000001 socket(AF_INET, SOCK_STREAM|SOCK_CLOEXEC|SOCK_NONBLOCK, IPPROTO_IP) = 8\n"
                + "2.000002 connect(8, {sa_family=AF_INET, sin_port=htons(42000), "
                + 'sin_addr=inet_addr("198.51.100.11")}, 16) = -1 EINPROGRESS (Operation now in progress)\n',
            )
            branch_path = self.write_manifest(root / "branch", complete)
            comparison, matches = compare(main_path, branch_path)

        self.assertTrue(matches)
        self.assertTrue(comparison["static_profiles_match"])
        self.assertFalse(comparison["observed_profiles_match"])
        self.assertEqual(len(comparison["only_main_observed"]), 1)
        self.assertEqual(comparison["only_main"], [])

    def test_merge_unions_profiles_from_independently_fresh_runs(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            first = self.write_manifest(root / "run-1", CONNECTED_SOCKET)
            second = self.write_manifest(
                root / "run-2",
                "2.000001 socket(AF_INET6, SOCK_DGRAM|SOCK_CLOEXEC, IPPROTO_IP) = 8\n"
                "2.000002 connect(8, {sa_family=AF_INET6, sin6_port=htons(42000), "
                'sin6_flowinfo=htonl(0), inet_pton(AF_INET6, "2001:db8::10"), '
                "sin6_scope_id=0}, 28) = 0\n",
            )

            merged = merge_manifests([first, second])

        self.assertEqual(merged["merged_run_count"], 2)
        self.assertEqual(merged["observed_socket_count"], 2)
        self.assertEqual(merged["unique_profile_count"], 2)
        self.assertEqual(len(merged["constructors"]), 2)


if __name__ == "__main__":
    unittest.main()
