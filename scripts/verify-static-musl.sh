#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 The superseedr Contributors
# SPDX-License-Identifier: GPL-3.0-or-later

set -euo pipefail
export LC_ALL=C

if [[ $# -lt 1 || $# -gt 2 ]]; then
  echo "Usage: $0 PATH_TO_MUSL_BINARY [amd64|arm64]" >&2
  exit 2
fi

# Override for cross-platform inspection, e.g. READELF=llvm-readelf.
readelf_tool="${READELF:-readelf}"
command -v "$readelf_tool" >/dev/null || {
  echo "Missing ELF inspector: $readelf_tool" >&2
  exit 1
}

binary="$1"
expected_arch="${2:-}"
case "$expected_arch" in
  ""|amd64|arm64) ;;
  *) echo "Unsupported architecture: $expected_arch" >&2; exit 2 ;;
esac
[[ -f "$binary" ]] || { echo "Missing binary: $binary" >&2; exit 1; }

# Capture before matching: a failed readelf must never count as a passing check.
header="$("$readelf_tool" -h "$binary")"
segments="$("$readelf_tool" -l "$binary")"
dynamic="$("$readelf_tool" -d "$binary")"
versions="$("$readelf_tool" -V "$binary")"

if ! grep -Eq 'Type:[[:space:]]+(EXEC|DYN)' <<<"$header"; then
  echo "Not an ELF executable: $binary" >&2
  exit 1
fi
case "$expected_arch" in
  amd64) machine_pattern='Machine:.*(Advanced Micro Devices X86-64|AMD x86-64)' ;;
  arm64) machine_pattern='Machine:.*AArch64' ;;
  "") machine_pattern='Machine:.*(Advanced Micro Devices X86-64|AMD x86-64|AArch64)' ;;
esac
if ! grep -Eq "$machine_pattern" <<<"$header"; then
  echo "Unexpected ELF architecture (expected ${expected_arch:-amd64 or arm64}): $binary" >&2
  exit 1
fi
if grep -Eq '(^|[[:space:]])INTERP([[:space:]]|$)' <<<"$segments"; then
  echo "Musl artifact unexpectedly requires a dynamic loader: $binary" >&2
  exit 1
fi
if grep -q '(NEEDED)' <<<"$dynamic"; then
  echo "Musl artifact unexpectedly requires shared libraries: $binary" >&2
  exit 1
fi
if grep -q 'GLIBC_' <<<"$versions"; then
  echo "Musl artifact unexpectedly requires glibc symbols: $binary" >&2
  exit 1
fi

echo "Static ELF verified (no interpreter, shared dependencies, or glibc versions): $binary"
