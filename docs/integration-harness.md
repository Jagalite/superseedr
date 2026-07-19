# Integration Harness

## Overview
This harness runs end-to-end interoperability tests in Docker.

Phase 1 scope is `superseedr -> superseedr`.
The architecture is adapter-based so qBittorrent/Transmission can be added without redesign.

## Components
- `integration_tests/docker/docker-compose.interop.yml`
  - `tracker`
  - `superseedr_seed`
  - `superseedr_leech`
- `integration_tests/harness/`
  - Docker orchestration
  - Runtime settings generation
  - Seed/leech scenario runner
  - Manifest-based validator
- `integration_tests/artifacts/generated_torrents/`
  - Runtime-normalized torrent metadata (local tracker announce URL)
- `integration_tests/artifacts/`
  - Raw client status snapshots
  - Normalized status timeline
  - Validator report
  - Container logs

## Monitoring Policy
- Progress and early failure signals:
  - Superseedr: `status_files/app_state.json`
  - Future clients: their HTTP/RPC API
- Final pass/fail gate:
  - Filesystem hash validation against `integration_tests/test_data`

Default polling profile is adaptive:
- 1s while state is changing
- 5s when stable

## Local Usage
Install dependencies:

```bash
python3 -m pip install -r requirements-integration.txt
```

Run all modes:

```bash
./integration_tests/run_interop.sh all
```

Run one mode directly:

```bash
python3 -m integration_tests.harness.run --scenario superseedr_to_superseedr --mode v2 --timeout-secs 300
```

The separate privileged Linux network-binding gate uses network namespaces and packet
captures to assert that strict traffic never reaches the clear/default interface:

```bash
sudo ./integration_tests/network_binding/run_netns_leak_test.sh
```

See [Native Network Binding](native-network-binding.md) for its topology, capability
requirements, and exact security assertion.

## CI Usage
Workflow: `.github/workflows/integration-interop.yml`

Triggers:
- Manual dispatch (`workflow_dispatch`)
- Nightly schedule

Artifacts are uploaded from `integration_tests/artifacts/` for each run.

## Extending to Other Clients
1. Implement adapter in `integration_tests/harness/clients/`.
2. Add client-specific telemetry polling and log collection.
3. Add scenario module in `integration_tests/harness/scenarios/`.
4. Add pytest case(s) in `integration_tests/harness/tests/`.
