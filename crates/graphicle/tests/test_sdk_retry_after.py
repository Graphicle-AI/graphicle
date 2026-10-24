"""A spent budget keeps Retry-After so the caller knows when to try again."""
import sys
import threading
from datetime import datetime, timedelta, timezone
from email.utils import format_datetime
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "sdk" / "python"))
from graphicle_sdk import Client, GraphicleError


class _Budget(BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'{"error":"monthly request budget is spent"}'
        self.send_response(429)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        if self.path == "/seconds":
            self.send_header("Retry-After", "3600")
        elif self.path == "/date":
            when = datetime.now(timezone.utc) + timedelta(hours=2)
            self.send_header("Retry-After", format_datetime(when, usegmt=True))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, _format, *_args):
        return


def _client():
    server = ThreadingHTTPServer(("127.0.0.1", 0), _Budget)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    port = server.server_address[1]
    client = Client(f"http://127.0.0.1:{port}", "workspace", "test-capability", timeout=5)
    return server, client


def test_retry_after_seconds_is_kept_on_the_error():
    server, client = _client()
    try:
        with pytest.raises(GraphicleError) as caught:
            client.request("GET", "/seconds")
    finally:
        server.shutdown()
    error = caught.value
    assert error.status == 429
    assert error.retry_after == 3600
    assert "3600" in str(error)
    assert "retry after" in str(error)


def test_retry_after_http_date_is_seconds_from_now():
    server, client = _client()
    try:
        with pytest.raises(GraphicleError) as caught:
            client.request("GET", "/date")
    finally:
        server.shutdown()
    error = caught.value
    assert 7000 <= error.retry_after <= 7400
    assert "retry after" in str(error)


def test_a_refusal_without_retry_after_stays_plain():
    server, client = _client()
    try:
        with pytest.raises(GraphicleError) as caught:
            client.request("GET", "/plain")
    finally:
        server.shutdown()
    error = caught.value
    assert error.retry_after is None
    assert "retry after" not in str(error)
