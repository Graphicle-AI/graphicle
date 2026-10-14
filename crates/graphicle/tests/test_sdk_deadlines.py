"""Job polling carries its remaining timeout into the HTTP request."""
import json
import sys
from pathlib import Path
from unittest.mock import Mock

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "sdk" / "python"))
from graphicle_sdk import GraphicleError, Client, Project
from graphicle_sdk import client as client_module


def test_wait_bounds_a_slow_status_request(monkeypatch):
    clock = [0.0]
    monkeypatch.setattr(client_module.time, "monotonic", lambda: clock[0])
    client = Client("http://127.0.0.1:8854", "workspace", "test-capability", timeout=60)
    observed = []

    def stalled(request, *, timeout):
        observed.append(timeout)
        assert json.loads(request.data)["arguments"]["repository_id"] == "project"
        clock[0] += timeout
        raise TimeoutError("status did not answer")

    client._opener = Mock()
    client._opener.open.side_effect = stalled
    with pytest.raises(TimeoutError, match="job-1"):
        Project(client, "project").wait("job-1", timeout=1)
    assert observed == [1]


def test_wait_does_not_start_a_request_after_its_deadline():
    client = Client("http://127.0.0.1:8854", "workspace", "test-capability")
    client._opener = Mock()
    with pytest.raises(TimeoutError, match="job-1"):
        Project(client, "project").wait("job-1", timeout=0)
    client._opener.open.assert_not_called()


def test_wait_preserves_connection_errors_before_deadline():
    client = Client("http://127.0.0.1:8854", "workspace", "test-capability")
    client._opener = Mock()
    client._opener.open.side_effect = ConnectionRefusedError("refused")
    with pytest.raises(GraphicleError, match="refused"):
        Project(client, "project").wait("job-1", timeout=1)


@pytest.mark.parametrize("options", [{"timeout": -1}, {"timeout": float("nan")},
                                     {"timeout": float("inf")}, {"interval": 0},
                                     {"interval": -1}, {"interval": float("nan")}])
def test_wait_rejects_invalid_polling_limits(options):
    client = Client("http://127.0.0.1:8854", "workspace", "test-capability")
    client._opener = Mock()
    with pytest.raises(ValueError):
        Project(client, "project").wait("job-1", **options)
    client._opener.open.assert_not_called()
