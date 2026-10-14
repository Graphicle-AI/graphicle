"""Authenticated SDK requests stay on their configured service origin."""
import sys
from pathlib import Path
from unittest.mock import Mock

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "sdk" / "python"))
from graphicle_sdk import Client, LocalServer


@pytest.mark.parametrize("path", ["@elsewhere.invalid/api", "//elsewhere.invalid/api", "https://elsewhere.invalid/api", "api", "/api#fragment", "/api\nignored"])
def test_request_rejects_non_origin_paths_before_sending(path):
    client = Client("https://service.invalid", "workspace", "test-capability")
    client._opener = Mock()
    with pytest.raises(ValueError):
        client.request("GET", path)
    client._opener.open.assert_not_called()


@pytest.mark.parametrize("url", ["https://user:password@service.invalid", "https://service.invalid/api", "https://service.invalid?next=", "https://service.invalid#fragment", "https://service.invalid:bad", "https://service.invalid\n"])
def test_client_requires_a_service_origin(url):
    with pytest.raises(ValueError):
        Client(url, "workspace", "test-capability")


def test_origin_path_keeps_query_and_workspace_capability():
    client = Client("https://service.invalid/", "workspace", "test-capability")
    response = Mock()
    response.read.return_value = b'{"ok": true}'
    client._opener = Mock()
    client._opener.open.return_value.__enter__ = Mock(return_value=response)
    client._opener.open.return_value.__exit__ = Mock(return_value=False)
    assert client.request("GET", "/api/items?limit=2") == {"ok": True}
    request = client._opener.open.call_args.args[0]
    assert request.full_url == "https://service.invalid/api/items?limit=2"
    assert request.get_header("X-workspace-capability") == "test-capability"


def test_local_server_keeps_an_explicit_relative_executable(tmp_path, monkeypatch):
    executable = tmp_path / "graphicle"
    executable.write_text("#!/bin/sh\nexit 0\n")
    executable.chmod(0o700)
    monkeypatch.chdir(tmp_path)
    server = LocalServer("./graphicle", tmp_path / "store")
    assert Path(server._command("init")[0]) == executable
