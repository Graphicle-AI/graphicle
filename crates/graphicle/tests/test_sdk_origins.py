"""Authenticated SDK requests stay on their configured service origin."""
import ipaddress
import sys
import urllib.request
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


@pytest.mark.parametrize("host", ["::ffff:127.0.0.1", "::ffff:7f00:2", "::ffff:127.255.255.254"])
def test_ipv4_mapped_loopback_is_loopback_on_every_python(host, monkeypatch):
    # Give IPv6Address the is_loopback of the Python releases that answer False for
    # IPv4-mapped addresses (3.12.3, for one), so the client's own reading is what is tested.
    monkeypatch.setattr(ipaddress.IPv6Address, "is_loopback", property(lambda address: int(address) == 1))
    assert not ipaddress.ip_address(host).is_loopback
    monkeypatch.setenv("http_proxy", "http://proxy.invalid:3128")

    def http_proxies(client):
        return [handler.proxies.get("http") for handler in client._opener.handlers
                if isinstance(handler, urllib.request.ProxyHandler)]

    # Plain http is allowed, and, as for every loopback address, the proxy settings are not
    # used; a host on the network (allowed with insecure_http) goes through the proxy.
    assert http_proxies(Client(f"http://[{host}]:8854", "workspace", "test-capability")) == []
    remote = Client("http://[::ffff:10.0.0.5]:8854", "workspace", "test-capability", insecure_http=True)
    assert http_proxies(remote) == ["http://proxy.invalid:3128"]


@pytest.mark.parametrize("host", ["[::ffff:10.0.0.5]", "[::127.0.0.1]", "localhost.", "127.0.0.1.example.com", "127.1"])
def test_plain_http_refuses_hosts_that_are_not_read_as_loopback(host):
    # [::127.0.0.1] is IPv4-compatible, not IPv4-mapped; 127.1 is no address to ipaddress.
    with pytest.raises(ValueError, match="loopback"):
        Client(f"http://{host}:8854", "workspace", "test-capability")


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
