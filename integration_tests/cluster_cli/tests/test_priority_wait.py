from types import SimpleNamespace
from unittest.mock import Mock, sentinel

import pytest

from integration_tests.cluster_cli import runner


pytestmark = pytest.mark.cluster_cli
INFO_HASH = "a" * 40
NODE = runner.ContainerNode("cluster_host_a", "host-a")


@pytest.fixture
def clock(monkeypatch):
    now = [0.0]

    def advance(seconds):
        now[0] += seconds

    clock = SimpleNamespace(monotonic=lambda: now[0], sleep=Mock(side_effect=advance))
    monkeypatch.setattr(runner, "time", clock)
    return clock


def info(priorities):
    return {"data": {"torrent": {"file_priorities": priorities}}}


@pytest.mark.parametrize("stale", [{}, {"0": "Low"}, {"1": "High"}])
def test_waits_for_persisted_priority(monkeypatch, clock, stale):
    read = Mock(side_effect=[info(stale), info(stale), info({"0": "High"})])
    monkeypatch.setattr(runner, "_docker_json", read)

    runner._wait_for_file_priority(sentinel.ctx, NODE, INFO_HASH, 0, "High", running=True)

    assert read.call_count == 3
    read.assert_called_with(sentinel.ctx, NODE.service, ["info", INFO_HASH], running=True)
    assert clock.sleep.call_count == 2


def test_missing_override_means_normal_priority(monkeypatch, clock):
    monkeypatch.setattr(runner, "_docker_json", Mock(return_value=info({})))

    runner._wait_for_file_priority(sentinel.ctx, NODE, INFO_HASH, 0, "Normal", running=False)

    clock.sleep.assert_not_called()


def test_priority_that_never_persists_still_fails(monkeypatch, clock):
    monkeypatch.setattr(runner, "_docker_json", Mock(return_value=info({"0": "Low"})))

    with pytest.raises(runner.ClusterCliError, match="Timed out.*file 0; last observed 'Low'"):
        runner._wait_for_file_priority(
            sentinel.ctx, NODE, INFO_HASH, 0, "High", running=True, timeout_secs=2.5
        )

    assert clock.monotonic() == 2.5


def test_cli_failure_is_not_retried(monkeypatch, clock):
    read = Mock(side_effect=runner.ClusterCliError("CLI failed"))
    monkeypatch.setattr(runner, "_docker_json", read)

    with pytest.raises(runner.ClusterCliError, match="CLI failed"):
        runner._wait_for_file_priority(sentinel.ctx, NODE, INFO_HASH, 0, "High", running=True)

    read.assert_called_once()
    clock.sleep.assert_not_called()
