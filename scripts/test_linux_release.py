#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 The superseedr Contributors
# SPDX-License-Identifier: GPL-3.0-or-later
"""Regression tests for release ABI checks and Debian dependency generation."""

import platform
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

from linux_release import control_field, fix_deb, glibc_versions, verify_dependency_baseline, with_dependencies


class MetadataTests(unittest.TestCase):
    def test_symbol_versions_are_numeric_and_deduplicated(self):
        self.assertEqual(glibc_versions("GLIBC_2.9 GLIBC_2.17 GLIBC_2.9 GLIBC_2.3.4"),
                         ["2.3.4", "2.9", "2.17"])

    def test_missing_and_private_versions_fail_closed(self):
        for value in ("", "GLIBC_PRIVATE", "GLIBC_2.17 GLIBC_ABI_DT_RELR"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                glibc_versions(value)

    def test_dependency_rewrite_preserves_other_fields_and_manual_dependencies(self):
        original = "Package: fixture-probe\nDepends: ca-certificates,\n libgcc-s1 (>= 3.0)\nDescription: A probe\n with a continuation\n"
        result = with_dependencies(original, "libc6 (>= 2.17), libgcc-s1 (>= 3.0)")
        self.assertEqual(control_field(result, "Depends"),
                         "ca-certificates, libgcc-s1 (>= 3.0), libc6 (>= 2.17)")
        self.assertIn("Description: A probe\n with a continuation\n", result)
        self.assertEqual(with_dependencies(result, "libc6 (>= 2.17), libgcc-s1 (>= 3.0)"), result)

    def test_missing_libc_dependency_is_rejected(self):
        for value in ("", "libgcc-s1 (>= 3.0)", "libc6"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                with_dependencies("Package: fixture-probe\n", value)

    def test_newer_package_metadata_cannot_raise_the_baseline(self):
        verify_dependency_baseline("libc6 (>= 2.17-1), libgcc-s1 (>= 3.0)", "2.17")
        for value in ("libc6 (>= 2.34)", "libc6", "libc6 (>= 2.17), libc6 (>= 2.31)"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                verify_dependency_baseline(value, "2.17")

    def test_dependency_must_cover_actual_binary_requirement(self):
        verify_dependency_baseline("libc6 (>= 2.17)", "2.17", "2.17")
        with self.assertRaisesRegex(ValueError, "does not cover binary requirement"):
            verify_dependency_baseline("libc6 (>= 2.12)", "2.17", "2.17")


@unittest.skipUnless(platform.system() == "Linux" and
                     all(shutil.which(tool) for tool in ("cc", "readelf", "dpkg-deb", "dpkg-shlibdeps")),
                     "requires Linux with Debian packaging tools")
class DebianIntegrationTests(unittest.TestCase):
    def test_real_package_gets_dependencies_and_rejects_wrong_payload(self):
        with tempfile.TemporaryDirectory() as temporary:
            work = Path(temporary)
            root = work / "root"
            (root / "usr/bin").mkdir(parents=True)
            (root / "DEBIAN").mkdir()
            architecture = subprocess.check_output(["dpkg", "--print-architecture"], text=True).strip()
            if architecture not in ("amd64", "arm64"):
                self.skipTest("release architectures only")
            (work / "probe.c").write_text('#include <stdio.h>\nint main(void) { return puts("fixture probe") < 0; }\n')
            binary = work / "probe"
            subprocess.run(["cc", str(work / "probe.c"), "-o", str(binary)], check=True)
            shutil.copy2(binary, root / "usr/bin/superseedr")
            (root / "DEBIAN/control").write_text(
                f"Package: fixture-probe\nVersion: 1.0\nArchitecture: {architecture}\n"
                "Maintainer: Fixture <fixture@example.invalid>\nDescription: Fixture probe\n"
            )
            package = work / "probe.deb"
            subprocess.run(["dpkg-deb", "--build", "--root-owner-group", str(root), str(package)], check=True)
            before = package.read_bytes()
            with self.assertRaises(ValueError):
                fix_deb(package, binary, "2.0", architecture)
            self.assertEqual(package.read_bytes(), before)
            fix_deb(package, binary, "99.0", architecture)
            dependencies = subprocess.check_output(["dpkg-deb", "-f", str(package), "Depends"], text=True)
            self.assertRegex(dependencies, r"libc6 \(>= [^)]+\)")
            # A package built from another executable must never inherit this one's metadata.
            with binary.open("ab") as output:
                output.write(b"different payload")
            before = package.read_bytes()
            with self.assertRaisesRegex(ValueError, "verified release binary"):
                fix_deb(package, binary, "99.0", architecture)
            self.assertEqual(package.read_bytes(), before)


if __name__ == "__main__":
    unittest.main()
