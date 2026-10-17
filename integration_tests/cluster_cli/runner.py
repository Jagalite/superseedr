from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import tomli_w

from integration_tests.cluster_cli.manifest import fixture_by_id, load_fixture_manifest
from integration_tests.harness.config import resolve_paths
from integration_tests.harness.docker_ctl import DockerCompose


CLUSTER_MOUNT_PATH = "/cluster"
CLUSTER_DOWNLOADS_PATH = "/cluster/downloads"
CLUSTER_SHARED_FIXTURES_PATH = "/cluster/shared-fixtures"
SERVICE_HOST_A = "cluster_host_a"
SERVICE_HOST_B = "cluster_host_b"
SERVICE_BOOTSTRAP = "cluster_bootstrap"
SERVICE_STANDALONE = "cluster_standalone"


class ClusterCliError(RuntimeError):
    pass


def _utc_stamp() -> str:
    return datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")


@dataclass(frozen=True)
class ContainerNode:
    service: str
    host_id: str


@dataclass
class ClusterRunContext:
    run_id: str
    artifacts_dir: Path
    runtime_root: Path
    restart_config_root: Path
    restart_share_root: Path
    host_a_config_root: Path
    host_a_share_root: Path
    host_b_config_root: Path
    host_b_share_root: Path
    compose: DockerCompose
    transcript: list[dict[str, Any]]
    host_a: ContainerNode
    host_b: ContainerNode


def run_cluster_cli_smoke(run_id: str | None = None, skip_build: bool = False) -> dict[str, Any]:
    run_id = run_id or f"cluster_cli_{_utc_stamp()}"
    ctx = _prepare_context(run_id)
    summary: dict[str, Any] = {
        "run_id": run_id,
        "artifacts_dir": str(ctx.artifacts_dir),
        "phases": [],
        "restart_regression": {},
    }

    try:
        ctx.compose.down()
        if not skip_build:
            ctx.compose.run(["build", SERVICE_HOST_A], check=True)
        _stage_fixtures(ctx)
        _seed_shared_config(ctx)

        summary["phases"].append(_phase_shared_offline(ctx))
        summary["phases"].append(_phase_single_online(ctx, no_build=skip_build))
        summary["phases"].append(_phase_cluster_online(ctx))
        summary["phases"].append(_phase_failover(ctx))
        summary["phases"].append(_phase_failback(ctx))
        summary["restart_regression"] = _restart_regression_check(ctx)
        summary["status"] = "ok"
        return summary
    finally:
        _capture_artifacts(ctx)
        (ctx.artifacts_dir / "transcript.json").write_text(
            json.dumps(ctx.transcript, indent=2),
            encoding="utf-8",
        )
        (ctx.artifacts_dir / "summary.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")
        ctx.compose.down()


def _prepare_context(run_id: str) -> ClusterRunContext:
    paths = resolve_paths()
    artifacts_dir = paths.artifacts_root / "cluster_cli" / run_id
    runtime_root = artifacts_dir / "runtime"
    host_a_config_root = runtime_root / "host-a" / "config"
    host_a_share_root = runtime_root / "host-a" / "share"
    host_b_config_root = runtime_root / "host-b" / "config"
    host_b_share_root = runtime_root / "host-b" / "share"
    restart_config_root = runtime_root / "standalone" / "config"
    restart_share_root = runtime_root / "standalone" / "share"
    for path in (
        artifacts_dir / "shared_snapshots",
        host_a_config_root,
        host_a_share_root,
        host_b_config_root,
        host_b_share_root,
        restart_config_root,
        restart_share_root,
    ):
        path.mkdir(parents=True, exist_ok=True)

    compose_file = paths.integration_root / "docker" / "docker-compose.cluster-cli.yml"
    project_name = f"clustercli{run_id.replace('-', '').replace('_', '')}".lower()
    env = {
        "CLUSTER_PROJECT_NAME": project_name,
        "CLUSTER_SHARED_VOLUME": f"{project_name}_shared_root",
        "CLUSTER_ARTIFACTS_ROOT": str(artifacts_dir),
        "CLUSTER_HOST_A_CONFIG": str(host_a_config_root),
        "CLUSTER_HOST_A_SHARE": str(host_a_share_root),
        "CLUSTER_HOST_B_CONFIG": str(host_b_config_root),
        "CLUSTER_HOST_B_SHARE": str(host_b_share_root),
        "CLUSTER_STANDALONE_CONFIG": str(restart_config_root),
        "CLUSTER_STANDALONE_SHARE": str(restart_share_root),
    }
    compose = DockerCompose(compose_file=compose_file, project_name=project_name, env=env)
    return ClusterRunContext(
        run_id=run_id,
        artifacts_dir=artifacts_dir,
        runtime_root=runtime_root,
        restart_config_root=restart_config_root,
        restart_share_root=restart_share_root,
        host_a_config_root=host_a_config_root,
        host_a_share_root=host_a_share_root,
        host_b_config_root=host_b_config_root,
        host_b_share_root=host_b_share_root,
        compose=compose,
        transcript=[],
        host_a=ContainerNode(service=SERVICE_HOST_A, host_id="host-a"),
        host_b=ContainerNode(service=SERVICE_HOST_B, host_id="host-b"),
    )


def _write_toml(path: Path, payload: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(tomli_w.dumps(payload), encoding="utf-8")


def _docker_json(
    ctx: ClusterRunContext,
    service: str,
    args: list[str],
    *,
    running: bool,
    extra_env: dict[str, str] | None = None,
) -> dict[str, Any]:
    env_args: list[str] = []
    if extra_env:
        for key, value in extra_env.items():
            env_args.extend(["-e", f"{key}={value}"])
    if running:
        docker_args = ["exec", "-T", *env_args, service, "superseedr", "--json", *args]
    else:
        docker_args = ["run", "--rm", "-T", *env_args, service, "--json", *args]
    result = ctx.compose.run(docker_args, check=False, capture=True)
    record = {
        "ts": _utc_stamp(),
        "service": service,
        "running": running,
        "args": args,
        "returncode": result.returncode,
        "stdout": result.stdout,
        "stderr": result.stderr,
    }
    ctx.transcript.append(record)
    try:
        payload = json.loads(result.stdout.strip())
    except json.JSONDecodeError as error:
        raise ClusterCliError(
            f"{service} did not return valid JSON for {' '.join(args)}:\n"
            f"stdout:\n{result.stdout}\n\nstderr:\n{result.stderr}"
        ) from error
    if result.returncode != 0 or not payload.get("ok", False):
        raise ClusterCliError(
            f"{service} CLI failed for {' '.join(args)}:\n"
            f"payload={json.dumps(payload, indent=2)}\n"
            f"stderr={result.stderr}"
        )
    return payload


def _compose_start(ctx: ClusterRunContext, services: list[str], *, no_build: bool = False) -> None:
    ctx.compose.up(services, no_build=no_build)


def _compose_stop(ctx: ClusterRunContext, service: str) -> None:
    ctx.compose.run(["stop", service], check=True)


def _snapshot_shared_root(ctx: ClusterRunContext, name: str) -> None:
    snapshot_root = ctx.artifacts_dir / "shared_snapshots" / name
    script = (
        f"set -e; rm -rf /artifacts/shared_snapshots/{name}; "
        f"mkdir -p /artifacts/shared_snapshots/{name}; "
        f"if [ -d /cluster/superseedr-config ]; then "
        f"cp -R /cluster/superseedr-config/. /artifacts/shared_snapshots/{name}/; "
        f"fi"
    )
    ctx.compose.run(
        ["run", "--rm", "--entrypoint", "sh", SERVICE_BOOTSTRAP, "-lc", script],
        check=True,
        capture=True,
    )


def _capture_artifacts(ctx: ClusterRunContext) -> None:
    _snapshot_shared_root(ctx, "final")
    logs_root = ctx.artifacts_dir / "logs"
    logs_root.mkdir(parents=True, exist_ok=True)
    for service in (SERVICE_HOST_A, SERVICE_HOST_B, SERVICE_STANDALONE):
        (logs_root / f"{service}.log").write_text(
            ctx.compose.logs(service, tail=1000),
            encoding="utf-8",
        )


def _stage_fixtures(ctx: ClusterRunContext) -> None:
    for fixture in load_fixture_manifest():
        script = (
            "set -e; mkdir -p /cluster/shared-fixtures; "
            f"cp /fixtures/{fixture.torrent_path.name} /cluster/shared-fixtures/{fixture.torrent_path.name}"
        )
        ctx.compose.run(
            ["run", "--rm", "--entrypoint", "sh", SERVICE_BOOTSTRAP, "-lc", script],
            check=True,
            capture=True,
        )


def _seed_shared_config(ctx: ClusterRunContext) -> None:
    fixture = fixture_by_id("single_4k_v1")
    payload_size = fixture.payload_path.stat().st_size
    standalone_settings = {
        "client_id": "cluster-smoke-host-a",
        "client_port": 6681,
        "default_download_folder": CLUSTER_DOWNLOADS_PATH,
        "torrents": [
            {
                "torrent_or_magnet": fixture.magnet_uri,
                "name": "single_4k.bin",
                "validation_status": False,
                "download_path": CLUSTER_DOWNLOADS_PATH,
            }
        ],
    }
    standalone_metadata = {
        "torrents": [
            {
                "info_hash_hex": fixture.info_hash_hex,
                "torrent_name": "single_4k.bin",
                "total_size": payload_size,
                "is_multi_file": False,
                "files": [
                    {
                        "relative_path": fixture.representative_relative_path,
                        "length": payload_size,
                    }
                ],
                "file_priorities": {},
            }
        ]
    }
    _write_toml(ctx.host_a_config_root / "settings.toml", standalone_settings)
    _write_toml(ctx.host_a_config_root / "torrent_metadata.toml", standalone_metadata)
    payload = _docker_json(ctx, SERVICE_BOOTSTRAP, ["to-shared", CLUSTER_MOUNT_PATH], running=False)
    selection = payload["data"]["selection"]
    if selection["mount_root"] != CLUSTER_MOUNT_PATH:
        raise ClusterCliError("to-shared returned an unexpected shared mount root")
def _phase_shared_offline(ctx: ClusterRunContext) -> dict[str, Any]:
    fixture = fixture_by_id("single_8k_v1")
    phase: dict[str, Any] = {"name": "shared_offline"}
    show_shared = _docker_json(ctx, SERVICE_HOST_A, ["show-shared-config"], running=False)
    if not show_shared["data"]["enabled"]:
        raise ClusterCliError("show-shared-config did not report shared mode enabled")
    if show_shared["data"]["selection"]["mount_root"] != CLUSTER_MOUNT_PATH:
        raise ClusterCliError("show-shared-config returned the wrong shared mount root")

    host_id = _docker_json(ctx, SERVICE_HOST_A, ["show-host-id"], running=False)
    if host_id["data"]["host_id"] != "host-a":
        raise ClusterCliError("show-host-id did not use the explicit host id")

    magnet_add = _docker_json(ctx, SERVICE_HOST_A, [fixture.magnet_uri], running=False)
    queued = magnet_add["data"]["queued"]
    if not queued:
        raise ClusterCliError("offline magnet add did not queue a command")
    queued_command = queued[0]["command_path"]
    if not queued_command.startswith(f"{CLUSTER_MOUNT_PATH}/superseedr-config/inbox/"):
        raise ClusterCliError("offline magnet add did not queue into the shared inbox")
    ctx.compose.run(
        ["run", "--rm", "--entrypoint", "sh", SERVICE_BOOTSTRAP, "-lc", f"rm -f '{queued_command}'"],
        check=True,
        capture=True,
    )

    _docker_json(ctx, SERVICE_HOST_A, ["pause", fixture_by_id("single_4k_v1").info_hash_hex], running=False)
    paused_torrents = _docker_json(ctx, SERVICE_HOST_A, ["torrents"], running=False)
    if paused_torrents["data"]["torrents"][0]["torrent_control_state"] != "Paused":
        raise ClusterCliError("offline pause did not persist the paused state")
    _docker_json(ctx, SERVICE_HOST_A, ["resume", fixture_by_id("single_4k_v1").info_hash_hex], running=False)
    resumed_torrents = _docker_json(ctx, SERVICE_HOST_A, ["torrents"], running=False)
    if resumed_torrents["data"]["torrents"][0]["torrent_control_state"] != "Running":
        raise ClusterCliError("offline resume did not persist the running state")

    phase["torrents"] = len(resumed_torrents["data"]["torrents"])
    _snapshot_shared_root(ctx, "phase_shared_offline")
    return phase


def _phase_single_online(ctx: ClusterRunContext, *, no_build: bool) -> dict[str, Any]:
    fixture = fixture_by_id("single_8k_v1")
    phase: dict[str, Any] = {"name": "single_online"}
    _compose_start(ctx, [SERVICE_HOST_A], no_build=no_build)
    _wait_for_leader(ctx, "host-a")
    _docker_json(ctx, SERVICE_HOST_A, ["status"], running=True)
    _docker_json(ctx, SERVICE_HOST_A, ["journal"], running=True)
    _docker_json(ctx, SERVICE_HOST_A, ["torrents"], running=True)

    _docker_json(
        ctx,
        SERVICE_HOST_A,
        ["add", f"{CLUSTER_SHARED_FIXTURES_PATH}/{fixture.torrent_path.name}"],
        running=True,
    )
    _wait_for_torrent_presence(ctx, ctx.host_a, fixture.info_hash_hex, True, running=True)
    info = _docker_json(ctx, SERVICE_HOST_A, ["info", fixture.info_hash_hex], running=True)
    files_payload = _wait_for_files(ctx, ctx.host_a, fixture.info_hash_hex, running=True)

    _docker_json(ctx, SERVICE_HOST_A, ["pause", fixture.info_hash_hex], running=True)
    _wait_for_control_state(ctx, ctx.host_a, fixture.info_hash_hex, "Paused", running=True)
    _docker_json(ctx, SERVICE_HOST_A, ["resume", fixture.info_hash_hex], running=True)
    _wait_for_control_state(ctx, ctx.host_a, fixture.info_hash_hex, "Running", running=True)
    _docker_json(
        ctx,
        SERVICE_HOST_A,
        ["priority", fixture.info_hash_hex, "--file-index", "0", "high"],
        running=True,
    )
    _wait_for_file_priority(ctx, ctx.host_a, fixture.info_hash_hex, 0, "High", running=True)
    _docker_json(ctx, SERVICE_HOST_A, ["remove", fixture.info_hash_hex], running=True)
    _wait_for_torrent_presence(ctx, ctx.host_a, fixture.info_hash_hex, False, running=True)

    phase["leader"] = "host-a"
    phase["files_count"] = len(files_payload["data"]["files"])
    phase["info_name"] = info["data"]["torrent"]["name"]
    _snapshot_shared_root(ctx, "phase_single_online")
    return phase


def _phase_cluster_online(ctx: ClusterRunContext) -> dict[str, Any]:
    fixture = fixture_by_id("single_16k_v1")
    seeded_fixture = fixture_by_id("single_4k_v1")
    phase: dict[str, Any] = {"name": "cluster_online"}
    _compose_start(ctx, [SERVICE_HOST_B], no_build=True)
    _wait_for_leader(ctx, "host-a")
    for command in ("show-shared-config", "show-host-id", "status", "journal", "torrents"):
        _docker_json(ctx, SERVICE_HOST_B, [command], running=True)

    add_payload = _docker_json(
        ctx,
        SERVICE_HOST_B,
        [fixture.magnet_uri],
        running=True,
    )
    if not add_payload["data"]["queued"]:
        raise ClusterCliError("follower add did not queue a shared add command")
    _wait_for_status_torrent_presence(ctx, ctx.host_a, fixture.info_hash_hex, True, running=True)

    _docker_json(ctx, SERVICE_HOST_B, ["pause", seeded_fixture.info_hash_hex], running=True)
    _wait_for_control_state(ctx, ctx.host_a, seeded_fixture.info_hash_hex, "Paused", running=True)
    _docker_json(ctx, SERVICE_HOST_B, ["resume", seeded_fixture.info_hash_hex], running=True)
    _wait_for_control_state(ctx, ctx.host_a, seeded_fixture.info_hash_hex, "Running", running=True)

    phase["leader"] = "host-a"
    phase["follower_add"] = fixture.info_hash_hex
    _snapshot_shared_root(ctx, "phase_cluster_online")
    return phase


def _phase_failover(ctx: ClusterRunContext) -> dict[str, Any]:
    fixture = fixture_by_id("single_4k_v1")
    phase: dict[str, Any] = {"name": "failover"}
    _compose_stop(ctx, SERVICE_HOST_A)
    _wait_for_leader(ctx, "host-b")

    for command in ("show-shared-config", "show-host-id", "status", "journal", "torrents"):
        _docker_json(ctx, SERVICE_HOST_A, [command], running=False)
    _docker_json(ctx, SERVICE_HOST_A, ["remove", fixture.info_hash_hex], running=False)
    _wait_for_torrent_presence(ctx, ctx.host_b, fixture.info_hash_hex, False, running=True)

    phase["leader"] = "host-b"
    _snapshot_shared_root(ctx, "phase_failover")
    return phase


def _phase_failback(ctx: ClusterRunContext) -> dict[str, Any]:
    fixture = fixture_by_id("single_25k_v1")
    surviving_fixture = fixture_by_id("single_16k_v1")
    phase: dict[str, Any] = {"name": "failback"}
    _compose_start(ctx, [SERVICE_HOST_A], no_build=True)
    _wait_for_leader(ctx, "host-b")
    _compose_stop(ctx, SERVICE_HOST_B)
    _wait_for_leader(ctx, "host-a")

    _docker_json(
        ctx,
        SERVICE_HOST_A,
        ["add", f"{CLUSTER_SHARED_FIXTURES_PATH}/{fixture.torrent_path.name}"],
        running=True,
    )
    _wait_for_torrent_presence(ctx, ctx.host_a, fixture.info_hash_hex, True, running=True)
    _docker_json(ctx, SERVICE_HOST_A, ["pause", fixture.info_hash_hex], running=True)
    _wait_for_control_state(ctx, ctx.host_a, fixture.info_hash_hex, "Paused", running=True)
    _docker_json(ctx, SERVICE_HOST_A, ["purge", fixture.info_hash_hex], running=True)
    _wait_for_torrent_presence(ctx, ctx.host_a, fixture.info_hash_hex, False, running=True)
    _docker_json(ctx, SERVICE_HOST_A, ["purge", surviving_fixture.info_hash_hex], running=True)
    _wait_for_torrent_presence(ctx, ctx.host_a, surviving_fixture.info_hash_hex, False, running=True)
    final_torrents = _docker_json(ctx, SERVICE_HOST_A, ["torrents"], running=True)
    if final_torrents["data"]["torrents"]:
        raise ClusterCliError("final failback cleanup did not leave an empty torrent list")

    phase["leader"] = "host-a"
    _snapshot_shared_root(ctx, "phase_failback")
    return phase


def _restart_regression_check(ctx: ClusterRunContext) -> dict[str, Any]:
    fixture = fixture_by_id("single_4k_v1")
    payload_size = fixture.payload_path.stat().st_size
    standalone_downloads = ctx.restart_share_root / "downloads"
    container_downloads = "/root/.local/share/jagalite.superseedr/downloads"
    standalone_downloads.mkdir(parents=True, exist_ok=True)
    shutil.copy2(fixture.payload_path, standalone_downloads / fixture.payload_path.name)
    _write_toml(
        ctx.restart_config_root / "settings.toml",
        {
            "client_id": "restart-regression",
            "client_port": 6683,
            "default_download_folder": container_downloads,
            "torrents": [
                {
                    "torrent_or_magnet": f"/fixtures/{fixture.torrent_path.name}",
                    "name": "single_4k.bin",
                    "validation_status": True,
                    "download_path": container_downloads,
                }
            ],
        },
    )
    _write_toml(
        ctx.restart_config_root / "torrent_metadata.toml",
        {
            "torrents": [
                {
                    "info_hash_hex": fixture.info_hash_hex,
                    "torrent_name": "single_4k.bin",
                    "total_size": payload_size,
                    "is_multi_file": False,
                    "files": [
                        {
                            "relative_path": fixture.representative_relative_path,
                            "length": payload_size,
                        }
                    ],
                    "file_priorities": {},
                }
            ]
        },
    )

    _compose_start(ctx, [SERVICE_STANDALONE], no_build=True)
    time.sleep(6)
    _compose_stop(ctx, SERVICE_STANDALONE)
    first_count = _standalone_completed_event_count(ctx)
    _compose_start(ctx, [SERVICE_STANDALONE], no_build=True)
    time.sleep(6)
    _compose_stop(ctx, SERVICE_STANDALONE)
    second_count = _standalone_completed_event_count(ctx)
    if second_count != first_count:
        raise ClusterCliError("Completed torrents were re-journaled on restart")
    return {"completed_event_count": first_count}


def _standalone_completed_event_count(ctx: ClusterRunContext) -> int:
    payload = _docker_json(ctx, SERVICE_STANDALONE, ["journal"], running=False)
    return sum(
        1 for entry in payload["data"]["entries"] if entry.get("event_type") == "TorrentCompleted"
    )


def _wait_for_leader(ctx: ClusterRunContext, expected_host_id: str, timeout_secs: float = 45.0) -> None:
    deadline = time.time() + timeout_secs
    while time.time() < deadline:
        result = ctx.compose.run(
            [
                "run",
                "--rm",
                "--entrypoint",
                "sh",
                SERVICE_BOOTSTRAP,
                "-lc",
                "if [ -f /cluster/superseedr-config/status/leader.json ]; then cat /cluster/superseedr-config/status/leader.json; fi",
            ],
            check=False,
            capture=True,
        )
        raw = result.stdout.strip()
        if raw:
            try:
                payload = json.loads(raw)
            except json.JSONDecodeError:
                time.sleep(1)
                continue
            if payload.get("status_config", {}).get("host_id") == expected_host_id:
                return
        time.sleep(1)
    raise ClusterCliError(f"Timed out waiting for leader '{expected_host_id}'")


def _wait_for_torrent_presence(
    ctx: ClusterRunContext,
    node: ContainerNode,
    info_hash_hex: str,
    should_exist: bool,
    *,
    running: bool,
    timeout_secs: float = 45.0,
) -> None:
    deadline = time.time() + timeout_secs
    while time.time() < deadline:
        payload = _docker_json(ctx, node.service, ["torrents"], running=running)
        found = any(torrent.get("info_hash_hex") == info_hash_hex for torrent in payload["data"]["torrents"])
        if found == should_exist:
            return
        time.sleep(1)
    raise ClusterCliError(
        f"Timed out waiting for torrent presence={should_exist} for {info_hash_hex}"
    )


def _wait_for_shared_path(
    ctx: ClusterRunContext,
    relative_path: str,
    *,
    timeout_secs: float = 45.0,
) -> None:
    deadline = time.time() + timeout_secs
    escaped_relative_path = relative_path.replace("'", "'\"'\"'")
    while time.time() < deadline:
        result = ctx.compose.run(
            [
                "run",
                "--rm",
                "--entrypoint",
                "sh",
                SERVICE_BOOTSTRAP,
                "-lc",
                f"test -f '/cluster/superseedr-config/{escaped_relative_path}'",
            ],
            check=False,
            capture=True,
        )
        if result.returncode == 0:
            return
        time.sleep(1)
    raise ClusterCliError(f"Timed out waiting for shared path '{relative_path}'")


def _wait_for_status_torrent_presence(
    ctx: ClusterRunContext,
    node: ContainerNode,
    info_hash_hex: str,
    should_exist: bool,
    *,
    running: bool,
    timeout_secs: float = 45.0,
) -> None:
    deadline = time.time() + timeout_secs
    while time.time() < deadline:
        payload = _docker_json(ctx, node.service, ["status"], running=running)
        torrents = payload["data"].get("torrents", {})
        found = info_hash_hex in torrents
        if found == should_exist:
            return
        time.sleep(1)
    raise ClusterCliError(
        f"Timed out waiting for status presence={should_exist} for {info_hash_hex}"
    )


def _wait_for_control_state(
    ctx: ClusterRunContext,
    node: ContainerNode,
    info_hash_hex: str,
    expected_state: str,
    *,
    running: bool,
    timeout_secs: float = 45.0,
) -> None:
    deadline = time.time() + timeout_secs
    while time.time() < deadline:
        payload = _docker_json(ctx, node.service, ["torrents"], running=running)
        for torrent in payload["data"]["torrents"]:
            if torrent.get("info_hash_hex") == info_hash_hex and torrent.get("torrent_control_state") == expected_state:
                return
        time.sleep(1)
    raise ClusterCliError(
        f"Timed out waiting for control state '{expected_state}' for {info_hash_hex}"
    )


def _wait_for_file_priority(
    ctx: ClusterRunContext,
    node: ContainerNode,
    info_hash_hex: str,
    file_index: int,
    expected_priority: str,
    *,
    running: bool,
    timeout_secs: float = 45.0,
) -> None:
    # Shared mutations are queued; info reads the persisted catalog, which can
    # still contain the old value after the command has been accepted.
    deadline = time.monotonic() + timeout_secs
    while True:
        payload = _docker_json(ctx, node.service, ["info", info_hash_hex], running=running)
        priorities = payload["data"]["torrent"]["file_priorities"]
        observed = priorities.get(str(file_index), "Normal")
        if observed == expected_priority:
            return
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise ClusterCliError(
                f"Timed out waiting for persisted file priority '{expected_priority}' "
                f"for {info_hash_hex} file {file_index}; last observed '{observed}'"
            )
        time.sleep(min(1.0, remaining))


def _wait_for_files(
    ctx: ClusterRunContext,
    node: ContainerNode,
    info_hash_hex: str,
    *,
    running: bool,
    timeout_secs: float = 45.0,
) -> dict[str, Any]:
    deadline = time.time() + timeout_secs
    while time.time() < deadline:
        payload = _docker_json(ctx, node.service, ["files", info_hash_hex], running=running)
        if payload["data"]["files"]:
            return payload
        time.sleep(1)
    raise ClusterCliError(f"Timed out waiting for files metadata for {info_hash_hex}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Run the Dockerized Superseedr cluster CLI smoke harness")
    parser.add_argument("--run-id", default=None, help="Optional explicit run id")
    parser.add_argument("--skip-build", action="store_true", help="Reuse an existing built image")
    args = parser.parse_args(argv)

    summary = run_cluster_cli_smoke(run_id=args.run_id, skip_build=args.skip_build)
    print(json.dumps(summary, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
