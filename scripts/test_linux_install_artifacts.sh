#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025 The superseedr Contributors
# SPDX-License-Identifier: GPL-3.0-or-later

set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/test_linux_install_artifacts.sh --artifact-dir DIR [--platform linux/arm64] [--image ubuntu:24.04]

Runs a Docker-based Linux install smoke test against Superseedr release artifacts.
The artifact directory must contain exactly one .deb and exactly one .tar.gz
for the variant being tested.

Checks:
  - artifact SHA256SUMS, when present
  - tarball extracts to a runnable ./superseedr binary before package installation
  - apt installs the .deb
  - dpkg registers package superseedr
  - /usr/bin/superseedr exists and starts with --help

Examples:
  scripts/test_linux_install_artifacts.sh --artifact-dir staging --platform linux/arm64
  scripts/test_linux_install_artifacts.sh --artifact-dir /tmp/artifact --platform linux/amd64 --image debian:bookworm
USAGE
}

artifact_dir=""
platform="linux/arm64"
image="ubuntu:24.04"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --artifact-dir)
      artifact_dir="${2:-}"
      shift 2
      ;;
    --platform)
      platform="${2:-}"
      shift 2
      ;;
    --image)
      image="${2:-}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ -z "$artifact_dir" ]]; then
  echo "--artifact-dir is required" >&2
  usage >&2
  exit 2
fi

if [[ ! -d "$artifact_dir" ]]; then
  echo "artifact directory does not exist: $artifact_dir" >&2
  exit 1
fi

artifact_dir="$(cd "$artifact_dir" && pwd)"

shopt -s nullglob
debs=("$artifact_dir"/*.deb)
tarballs=("$artifact_dir"/*.tar.gz)
shopt -u nullglob

if [[ "${#debs[@]}" -ne 1 ]]; then
  echo "expected exactly one .deb in $artifact_dir, found ${#debs[@]}" >&2
  printf '  %s\n' "${debs[@]}" >&2
  exit 1
fi

if [[ "${#tarballs[@]}" -ne 1 ]]; then
  echo "expected exactly one .tar.gz in $artifact_dir, found ${#tarballs[@]}" >&2
  printf '  %s\n' "${tarballs[@]}" >&2
  exit 1
fi

if ! command -v docker >/dev/null 2>&1; then
  echo "docker is required for the Linux artifact install test" >&2
  exit 1
fi

echo "Linux artifact install test directory: $artifact_dir"
echo "Docker image: $image"
echo "Docker platform: $platform"
echo "Debian package: $(basename "${debs[0]}")"
echo "Tarball: $(basename "${tarballs[0]}")"

docker run --rm \
  --platform "$platform" \
  -v "$artifact_dir:/artifacts:ro" \
  "$image" \
  bash -lc '
    set -euo pipefail
    export DEBIAN_FRONTEND=noninteractive

    echo "== system =="
    uname -a
    dpkg --print-architecture

    echo "== artifact layout =="
    find /artifacts -maxdepth 1 -type f -printf "%f %s bytes\n" | sort

    if [ -f /artifacts/SHA256SUMS ]; then
      echo "== checksums =="
      (cd /artifacts && sha256sum -c SHA256SUMS)
    fi

    deb=$(find /artifacts -maxdepth 1 -name "*.deb" -print -quit)
    tarball=$(find /artifacts -maxdepth 1 -name "*.tar.gz" -print -quit)

    echo "== deb metadata =="
    dpkg-deb --info "$deb"
    dpkg-deb --contents "$deb"

    echo "== smoke-test tarball before installing deb =="
    apt-get update
    apt-get install -y file
    mkdir -p /tmp/superseedr-tarball
    tar -xzf "$tarball" -C /tmp/superseedr-tarball
    tarball_bin=$(find /tmp/superseedr-tarball -type f -name superseedr -perm /111 -print -quit)
    test -n "$tarball_bin"
    file "$tarball_bin"
    ldd "$tarball_bin"
    "$tarball_bin" --help >/tmp/superseedr-tarball-help.txt
    sed -n "1,40p" /tmp/superseedr-tarball-help.txt

    echo "== install deb =="
    apt-get install -y "$deb"
    # HTTP client construction needs a system trust store. The package must
    # bring it in; preinstalling it above would hide a missing dependency.
    test -s /etc/ssl/certs/ca-certificates.crt

    echo "== installed package =="
    dpkg -s superseedr
    dpkg -L superseedr
    test -x /usr/bin/superseedr
    file /usr/bin/superseedr
    ldd /usr/bin/superseedr
    superseedr --help >/tmp/superseedr-help.txt
    sed -n "1,40p" /tmp/superseedr-help.txt

    echo "== uninstall =="
    apt-get purge -y superseedr
    ! dpkg-query -W superseedr >/dev/null 2>&1
    test ! -e /usr/bin/superseedr
  '
