#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 The superseedr Contributors
# SPDX-License-Identifier: GPL-3.0-or-later
"""Check GNU ELF compatibility and populate dependencies in a built Debian package."""

import argparse
import hashlib
import os
from pathlib import Path
import re
import subprocess
import tempfile


def run(*args, cwd=None):
    return subprocess.check_output(args, cwd=cwd, text=True, env={**os.environ, "LC_ALL": "C"})


def glibc_versions(output):
    names = set(re.findall(r"\bGLIBC_([A-Za-z0-9_.]+)", output))
    if not names or any(not re.fullmatch(r"\d+(?:\.\d+)+", name) for name in names):
        raise ValueError("expected numeric GLIBC symbol versions; missing or private ABI requirements")
    return sorted(names, key=lambda value: tuple(map(int, value.split("."))))


def verify_glibc(binary, maximum, architecture):
    readelf = os.environ.get("READELF", "readelf")
    header = run(readelf, "-h", str(binary))
    machine = {"amd64": "Advanced Micro Devices X86-64", "arm64": "AArch64"}[architecture]
    if not re.search(r"Machine:\s+" + re.escape(machine) + r"\s*$", header, re.M):
        raise ValueError(f"ELF architecture does not match {architecture}")
    versions = glibc_versions(run(readelf, "-V", str(binary)))
    if tuple(map(int, versions[-1].split("."))) > tuple(map(int, maximum.split("."))):
        raise ValueError(f"{binary} requires GLIBC_{versions[-1]}, exceeds {maximum}")
    print(f"{binary}: {architecture}, highest GLIBC requirement {versions[-1]} (limit {maximum})")
    return versions[-1]


def control_field(control, name):
    match = re.search(r"^" + re.escape(name) + r":([^\n]*(?:\n[ \t][^\n]*)*)", control, re.M)
    return " ".join(match.group(1).split()) if match else ""


def with_dependencies(control, generated):
    if not re.search(r"(?:^|,\s*)libc6(?:\:[a-z0-9]+)?\s*\(>=\s*[^)]+\)", generated):
        raise ValueError("dpkg-shlibdeps did not produce a versioned libc6 dependency")
    # Preserve any explicit non-library dependencies provided by cargo-bundle.
    existing = control_field(control, "Depends")
    dependencies = ", ".join(dict.fromkeys(part.strip() for part in (existing + "," + generated).split(",") if part.strip()))
    updated = re.sub(r"^Depends:[^\n]*(?:\n[ \t][^\n]*)*\n?", "", control, flags=re.M)
    return "Depends: " + dependencies + "\n" + updated


def verify_dependency_baseline(dependencies, maximum, minimum=None):
    # A newer distribution's shlibs metadata must not silently undo the ABI target.
    versions = re.findall(r"(?:^|,\s*)libc6(?:\:[a-z0-9]+)?\s*\(>=\s*(\d+(?:\.\d+)+)[^)]*\)", dependencies)
    if not versions or any(tuple(map(int, version.split("."))) > tuple(map(int, maximum.split(".")))
                           for version in versions):
        raise ValueError(f"Debian libc6 dependency exceeds or does not specify baseline {maximum}: {dependencies}")
    if minimum and max(tuple(map(int, version.split("."))) for version in versions) < tuple(map(int, minimum.split("."))):
        raise ValueError(f"Debian libc6 dependency does not cover binary requirement {minimum}: {dependencies}")


def digest(path):
    checksum = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            checksum.update(chunk)
    return checksum.hexdigest()


def fix_deb(package, binary, maximum, architecture):
    package, binary = package.resolve(), binary.resolve()
    minimum = verify_glibc(binary, maximum, architecture)
    with tempfile.TemporaryDirectory(prefix="superseedr-deb-", dir=package.parent) as temporary:
        work = Path(temporary)
        root = work / "root"
        run("dpkg-deb", "--raw-extract", str(package), str(root))
        packaged_binary = root / "usr/bin/superseedr"
        if digest(packaged_binary) != digest(binary):
            raise ValueError("Debian package does not contain the verified release binary")
        control_path = root / "DEBIAN/control"
        control = control_path.read_text()
        if control_field(control, "Architecture") != architecture:
            raise ValueError("Debian package architecture does not match the release target")
        (work / "debian").mkdir()
        (work / "debian/control").write_text(
            "Source: superseedr\nSection: net\nPriority: optional\n"
            "Maintainer: Release Builder <builder@example.invalid>\n\n"
            "Package: superseedr\nArchitecture: any\nDescription: Release dependency inspection\n"
        )
        output = run("dpkg-shlibdeps", "-O", "-e" + str(packaged_binary), cwd=work)
        dependencies = next((line.split("=", 1)[1] for line in output.splitlines()
                             if line.startswith("shlibs:Depends=")), "")
        control_path.write_text(with_dependencies(control, dependencies))
        verify_dependency_baseline(control_field(control_path.read_text(), "Depends"), maximum, minimum)
        rebuilt = work / "result.deb"
        run("dpkg-deb", "--build", "--root-owner-group", str(root), str(rebuilt))
        final = run("dpkg-deb", "--field", str(rebuilt), "Depends").strip()
        if final != control_field(control_path.read_text(), "Depends"):
            raise ValueError("rebuilt package dependency verification failed")
        os.replace(rebuilt, package)
        print(f"{package.name}: Depends: {final}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["verify-glibc", "fix-deb"])
    parser.add_argument("binary", type=Path)
    parser.add_argument("--max-glibc", required=True)
    parser.add_argument("--architecture", choices=["amd64", "arm64"], required=True)
    parser.add_argument("--deb", type=Path)
    args = parser.parse_args()
    if args.command == "fix-deb":
        if args.deb is None:
            parser.error("fix-deb requires --deb")
        fix_deb(args.deb, args.binary, args.max_glibc, args.architecture)
    else:
        verify_glibc(args.binary, args.max_glibc, args.architecture)


if __name__ == "__main__":
    main()
