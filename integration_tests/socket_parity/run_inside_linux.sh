#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025 The superseedr Contributors
# SPDX-License-Identifier: GPL-3.0-or-later

set -euo pipefail

: "${SOCKET_PARITY_LIVE_TORRENT_URL:?Set SOCKET_PARITY_LIVE_TORRENT_URL to a public live torrent URL}"
: "${SOCKET_PARITY_MAIN_SHA:?Missing main revision metadata}"
: "${SOCKET_PARITY_BRANCH_SHA:?Missing branch revision metadata}"

artifact_root=/artifacts
state_root=/state
live_torrent=/state/live-input.torrent
run_seconds=${SOCKET_PARITY_RUN_SECONDS:-20}
run_repeats=${SOCKET_PARITY_RUN_REPEATS:-2}
if (( run_repeats < 1 )); then
  echo "SOCKET_PARITY_RUN_REPEATS must be at least 1." >&2
  exit 2
fi
mkdir -p "${artifact_root}" "${state_root}"

{
  printf 'main_sha=%s\n' "${SOCKET_PARITY_MAIN_SHA}"
  printf 'branch_sha=%s\n' "${SOCKET_PARITY_BRANCH_SHA}"
  printf 'torrent_url=%s\n' "${SOCKET_PARITY_LIVE_TORRENT_URL}"
  printf 'run_seconds=%s\n' "${run_seconds}"
  printf 'run_repeats=%s\n' "${run_repeats}"
  printf 'kernel='
  uname -a
  printf 'os_release_begin\n'
  sed -n '1,80p' /etc/os-release
  printf 'os_release_end\n'
} >"${artifact_root}/environment.txt"

curl --fail --location --silent --show-error \
  --output "${live_torrent}" \
  "${SOCKET_PARITY_LIVE_TORRENT_URL}"
sha256sum "${live_torrent}" >"${artifact_root}/live-input.sha256"

run_revision() {
  local revision=$1
  local binary=$2
  local run_index=$3
  local label="${revision}-run-${run_index}"
  local revision_root="${state_root}/${label}"
  local shared_root="${revision_root}/shared"
  local home_root="${revision_root}/home"
  local output_root="${artifact_root}/${revision}/run-${run_index}"

  if [[ -e ${revision_root} ]]; then
    echo "Refusing to reuse state root: ${revision_root}" >&2
    return 1
  fi
  mkdir -p "${shared_root}" "${home_root}/config" "${home_root}/data" "${output_root}/raw"
  find "${shared_root}" -mindepth 1 -print -quit | grep -q . && {
    echo "Fresh shared root was unexpectedly non-empty: ${shared_root}" >&2
    return 1
  }
  {
    printf 'path=%s\n' "${shared_root}"
    printf 'existed_before_run=false\n'
    printf 'initial_entry_count=0\n'
  } >"${output_root}/fresh-shared-root.txt"

  local -a run_env=(
    env
    "HOME=${home_root}"
    "XDG_CONFIG_HOME=${home_root}/config"
    "XDG_DATA_HOME=${home_root}/data"
    "SUPERSEEDR_SHARED_CONFIG_DIR=${shared_root}"
    "SUPERSEEDR_SHARED_HOST_ID=socket-parity-host"
    "SUPERSEEDR_CLIENT_PORT=16881"
    "SUPERSEEDR_OUTPUT_STATUS_INTERVAL=2"
    "SUPERSEEDR_DEFAULT_DOWNLOAD_FOLDER=${shared_root}/downloads"
    "RUST_LOG=warn"
  )

  "${run_env[@]}" "${binary}" --json show-configs --all \
    >"${output_root}/show-configs.json"

  set +e
  script -qefc \
    "${run_env[*]} timeout --foreground --signal=INT --kill-after=3s ${run_seconds}s strace -ff -qq -ttt -s 512 -o ${output_root}/raw/trace -e trace=socket,setsockopt,bind,connect,listen,accept,accept4,fcntl,ioctl,dup,dup2,dup3,close ${binary}" \
    /dev/null >"${output_root}/tui.stdout" 2>"${output_root}/tui.stderr" &
  local trace_pid=$!
  set -e

  local status_path="${shared_root}/superseedr-config/hosts/socket-parity-host/status.json"
  local ready=false
  for _ in $(seq 1 40); do
    if [[ -f ${status_path} ]]; then
      ready=true
      break
    fi
    sleep 0.25
  done
  if [[ ${ready} != true ]]; then
    echo "${label} did not publish a status snapshot during startup" >&2
    kill "${trace_pid}" 2>/dev/null || true
    wait "${trace_pid}" 2>/dev/null || true
    return 1
  fi

  mkdir -p "${shared_root}/downloads"
  if ! "${run_env[@]}" "${binary}" --json add \
    --path "${shared_root}/downloads" "${live_torrent}" \
    >"${output_root}/add.json"; then
    kill "${trace_pid}" 2>/dev/null || true
    wait "${trace_pid}" 2>/dev/null || true
    return 1
  fi

  set +e
  wait "${trace_pid}"
  local run_status=$?
  set -e
  if [[ ${run_status} -ne 0 && ${run_status} -ne 124 && ${run_status} -ne 130 && ${run_status} -ne 137 ]]; then
    echo "${label} live run exited unexpectedly with status ${run_status}" >&2
    return "${run_status}"
  fi
  printf '%s\n' "${run_status}" >"${output_root}/run-status.txt"

  python3 /opt/socket-parity/normalize_strace.py normalize \
    "${output_root}/raw" "${output_root}/manifest.json"
}

for run_index in $(seq 1 "${run_repeats}"); do
  run_revision main /opt/socket-parity/main-superseedr "${run_index}"
done
for run_index in $(seq 1 "${run_repeats}"); do
  run_revision branch /opt/socket-parity/branch-superseedr "${run_index}"
done

python3 /opt/socket-parity/normalize_strace.py merge \
  "${artifact_root}/main/manifest.json" \
  "${artifact_root}"/main/run-*/manifest.json
python3 /opt/socket-parity/normalize_strace.py merge \
  "${artifact_root}/branch/manifest.json" \
  "${artifact_root}"/branch/run-*/manifest.json

python3 /opt/socket-parity/normalize_strace.py compare \
  "${artifact_root}/main/manifest.json" \
  "${artifact_root}/branch/manifest.json" \
  "${artifact_root}/comparison.json"

echo "Socket parity artifacts written to ${artifact_root}."
