#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025 The superseedr Contributors
# SPDX-License-Identifier: GPL-3.0-or-later

set -euo pipefail

for command in docker git tar; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "Missing required command: ${command}" >&2
    exit 2
  fi
done

: "${SUPERSEEDR_SOCKET_PARITY_TORRENT_URL:?Set SUPERSEEDR_SOCKET_PARITY_TORRENT_URL to an official Linux ISO torrent URL}"

repo_root=$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)
main_ref=${SUPERSEEDR_SOCKET_PARITY_MAIN_REF:-origin/main}
branch_ref=${SUPERSEEDR_SOCKET_PARITY_BRANCH_REF:-HEAD}
main_sha=$(git -C "${repo_root}" rev-parse "${main_ref}^{commit}")
branch_sha=$(git -C "${repo_root}" rev-parse "${branch_ref}^{commit}")
merge_base=$(git -C "${repo_root}" merge-base "${main_sha}" "${branch_sha}")
if [[ ${merge_base} != "${main_sha}" ]]; then
  echo "Main reference ${main_sha} is not an ancestor of branch ${branch_sha}." >&2
  exit 2
fi

run_id=$(date -u +%Y%m%dT%H%M%SZ)
artifact_root=${SUPERSEEDR_SOCKET_PARITY_ARTIFACT_DIR:-${repo_root}/integration_tests/artifacts/socket-parity-${run_id}}
build_context=$(mktemp -d "${TMPDIR:-/tmp}/superseedr-socket-parity.XXXXXX")
cleanup() {
  rm -rf -- "${build_context}"
}
trap cleanup EXIT

mkdir -p "${artifact_root}" "${build_context}/main" "${build_context}/branch" "${build_context}/harness"
git -C "${repo_root}" archive --format=tar --output="${build_context}/main.tar" "${main_sha}"
git -C "${repo_root}" archive --format=tar --output="${build_context}/branch.tar" "${branch_sha}"
tar -xf "${build_context}/main.tar" -C "${build_context}/main"
tar -xf "${build_context}/branch.tar" -C "${build_context}/branch"
cp "${repo_root}/integration_tests/socket_parity/Dockerfile" "${build_context}/Dockerfile"
cp "${repo_root}/integration_tests/socket_parity/normalize_strace.py" "${build_context}/harness/normalize_strace.py"
cp "${repo_root}/integration_tests/socket_parity/run_inside_linux.sh" "${build_context}/harness/run_inside_linux.sh"

image_tag="superseedr-socket-parity:${branch_sha:0:12}"
docker build \
  --build-arg "MAIN_SHA=${main_sha}" \
  --build-arg "BRANCH_SHA=${branch_sha}" \
  --file "${build_context}/Dockerfile" \
  --tag "${image_tag}" \
  "${build_context}"

docker image inspect "${image_tag}" \
  --format '{{json .RepoDigests}} {{json .Os}} {{json .Architecture}} {{json .Id}}' \
  >"${artifact_root}/image.txt"

docker run --rm \
  --env "SOCKET_PARITY_LIVE_TORRENT_URL=${SUPERSEEDR_SOCKET_PARITY_TORRENT_URL}" \
  --env "SOCKET_PARITY_RUN_SECONDS=${SUPERSEEDR_SOCKET_PARITY_RUN_SECONDS:-20}" \
  --env "SOCKET_PARITY_RUN_REPEATS=${SUPERSEEDR_SOCKET_PARITY_RUN_REPEATS:-2}" \
  --volume "${artifact_root}:/artifacts" \
  "${image_tag}"

echo "Compared main ${main_sha} with branch ${branch_sha}."
echo "Artifacts: ${artifact_root}"
