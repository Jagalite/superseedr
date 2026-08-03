#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2025 The superseedr Contributors
# SPDX-License-Identifier: GPL-3.0-or-later

set -euo pipefail

if [[ ${EUID} -ne 0 ]]; then
  echo "Run this privileged Linux integration test with sudo." >&2
  exit 2
fi

for command in cargo ip python3 tcpdump; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "Missing required command: ${command}" >&2
    exit 2
  fi
done

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
run_id=$$
client_ns="ss-client-${run_id}"
vpn_ns="ss-vpn-${run_id}"
clear_ns="ss-clear-${run_id}"
artifact_dir="${repo_root}/integration_tests/artifacts/network-binding-${run_id}"
vpn_capture="${artifact_dir}/vpn.pcap"
clear_capture="${artifact_dir}/clear.pcap"
peer_pid=""
vpn_capture_pid=""
clear_capture_pid=""

cleanup() {
  set +e
  [[ -n ${peer_pid} ]] && kill "${peer_pid}" 2>/dev/null
  [[ -n ${vpn_capture_pid} ]] && kill "${vpn_capture_pid}" 2>/dev/null
  [[ -n ${clear_capture_pid} ]] && kill "${clear_capture_pid}" 2>/dev/null
  ip netns delete "${client_ns}" 2>/dev/null
  ip netns delete "${vpn_ns}" 2>/dev/null
  ip netns delete "${clear_ns}" 2>/dev/null
}
trap cleanup EXIT

mkdir -p "${artifact_dir}"
cd "${repo_root}"
if [[ -n ${SUPERSEEDR_NETNS_TEST_BINARY:-} ]]; then
  test_binary=${SUPERSEEDR_NETNS_TEST_BINARY}
else
  test_binary=$(cargo test --locked --lib --all-features --no-run --message-format=json \
    | python3 -c '
import json
import sys

executables = []
for line in sys.stdin:
    artifact = json.loads(line)
    if (
        artifact.get("reason") == "compiler-artifact"
        and artifact.get("target", {}).get("name") == "superseedr"
        and artifact.get("profile", {}).get("test")
        and artifact.get("executable") is not None
    ):
        executables.append(artifact["executable"])

if len(executables) != 1:
    sys.exit(f"Expected one superseedr library test harness, found {len(executables)}")
print(executables[0])
')
fi
if [[ -z ${test_binary} ]]; then
  echo "Could not locate the compiled Rust test binary." >&2
  exit 1
fi
test_binary=$(realpath "${test_binary}")
if [[ ! -x ${test_binary} ]]; then
  echo "Rust test binary is not executable: ${test_binary}" >&2
  exit 1
fi
if ! "${test_binary}" --list --format terse \
  | grep -Fx 'networking::runtime::tests::linux_network_namespace_strict_binding_probe: test' >/dev/null; then
  echo "Rust test binary does not contain the strict network namespace probe." >&2
  exit 1
fi

ip netns add "${client_ns}"
ip netns add "${vpn_ns}"
ip netns add "${clear_ns}"
ip link add ss-vpn-client type veth peer name ss-vpn-peer
ip link add ss-clear-client type veth peer name ss-clear-peer
ip link set ss-vpn-client netns "${client_ns}"
ip link set ss-vpn-peer netns "${vpn_ns}"
ip link set ss-clear-client netns "${client_ns}"
ip link set ss-clear-peer netns "${clear_ns}"

ip -n "${client_ns}" link set lo up
ip -n "${client_ns}" link set ss-vpn-client name vpn0
ip -n "${client_ns}" addr add 198.18.0.1/30 dev vpn0
ip -n "${client_ns}" link set vpn0 up
ip -n "${client_ns}" link set ss-clear-client name clear0
ip -n "${client_ns}" addr add 198.18.0.5/30 dev clear0
ip -n "${client_ns}" link set clear0 up
ip -n "${client_ns}" route add default via 198.18.0.6 dev clear0

ip -n "${vpn_ns}" link set lo up
ip -n "${vpn_ns}" link set ss-vpn-peer name vpnpeer0
ip -n "${vpn_ns}" addr add 198.18.0.2/30 dev vpnpeer0
ip -n "${vpn_ns}" link set vpnpeer0 up

ip -n "${clear_ns}" link set lo up
ip -n "${clear_ns}" link set ss-clear-peer name clearpeer0
ip -n "${clear_ns}" addr add 198.18.0.6/30 dev clearpeer0
ip -n "${clear_ns}" link set clearpeer0 up

ip netns exec "${vpn_ns}" python3 \
  "${repo_root}/integration_tests/network_binding/netns_peer.py" &
peer_pid=$!
vpn_probe_filter='host 198.18.0.1 and host 198.18.0.2 and (tcp port 8080 or udp port 8081 or udp port 5353)'
clear_probe_filter='host 198.18.0.5 and host 198.18.0.6 and (tcp port 9090 or udp port 9090)'
ip netns exec "${vpn_ns}" tcpdump -U -n -i vpnpeer0 -w "${vpn_capture}" "${vpn_probe_filter}" &
vpn_capture_pid=$!
ip netns exec "${clear_ns}" tcpdump -U -n -i clearpeer0 -w "${clear_capture}" "${clear_probe_filter}" &
clear_capture_pid=$!
sleep 1

ip netns exec "${client_ns}" env \
  SUPERSEEDR_NETNS_INTERFACE=vpn0 \
  SUPERSEEDR_NETNS_TCP_TARGET=198.18.0.2:8080 \
  SUPERSEEDR_NETNS_UDP_TARGET=198.18.0.2:8081 \
  SUPERSEEDR_NETNS_CLEAR_TARGET=198.18.0.6:9090 \
  SUPERSEEDR_NETNS_DNS_SERVER=198.18.0.2:5353 \
  SUPERSEEDR_NETNS_DNS_HOST=probe.invalid \
  "${test_binary}" \
  networking::runtime::tests::linux_network_namespace_strict_binding_probe \
  --ignored --exact

kill "${vpn_capture_pid}" "${clear_capture_pid}"
wait "${vpn_capture_pid}" "${clear_capture_pid}" 2>/dev/null || true
vpn_capture_pid=""
clear_capture_pid=""

vpn_packets=$(tcpdump -n -r "${vpn_capture}" "${vpn_probe_filter}" 2>/dev/null | wc -l)
clear_packets=$(tcpdump -n -r "${clear_capture}" "${clear_probe_filter}" 2>/dev/null | wc -l)
if (( vpn_packets == 0 )); then
  echo "FAIL: the selected interface captured no probe traffic." >&2
  exit 1
fi
if (( clear_packets != 0 )); then
  echo "FAIL: ${clear_packets} packet(s) appeared on the clear/default interface." >&2
  tcpdump -n -r "${clear_capture}" "${clear_probe_filter}" 2>/dev/null >&2
  exit 1
fi

echo "PASS: ${vpn_packets} selected-interface packets; zero clear-interface packets."
echo "Captures: ${artifact_dir}"
