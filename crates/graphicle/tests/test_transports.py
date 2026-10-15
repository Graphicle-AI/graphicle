"""Real CLI/HTTP/MCP processes against deterministic, paginated API fixtures.

These qualify transport, boundaries and persistence, not model accuracy or
permission parity with a live SaaS tenant. No external API is called.
"""
import asyncio
import base64
import json
import os
from pathlib import Path
import signal
import subprocess
import threading
import time
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.error import HTTPError
from urllib.parse import parse_qs, urlparse
from urllib.request import Request, urlopen

import pytest

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("GRAPHICLE_BIN", ROOT / "target/debug/graphicle"))


class Service:
    def __init__(self, root):
        self.root = root
        self.data = root / "store"
        self.folder = root / "sources"
        self.folder.mkdir()
        (self.folder / "notes.txt").write_text("Mira prefers quiet rooms.\nUse the north entrance.\n")
        self.env = dict(os.environ, GRAPHICLE_FIXTURE_CREDENTIAL="synthetic-test-credential")
        self.config = root / "models.json"
        self.config.write_text("{}")
        self.process = None
        self.cli("init")
        self.cli("sync", str(self.folder))
        self.session = json.loads((self.data / "local-session.json").read_text())

    def command_line(self, *arguments):
        return [str(BINARY), "--data-dir", str(self.data), "--config", str(self.config), *arguments]

    def cli(self, *arguments, ok=True):
        result = subprocess.run(self.command_line(*arguments), env=self.env, text=True,
                                capture_output=True, timeout=90)
        if ok:
            assert result.returncode == 0, result.stderr
            return json.loads(result.stdout)
        assert result.returncode != 0
        return result.stderr

    def call(self, operation, **arguments):
        path = self.root / "arguments.json"
        path.write_text(json.dumps(arguments))
        return self.cli("call", operation, "--arguments", str(path))

    def job(self, kind, **arguments):
        queued = self.call("workflow.enqueue", kind=kind, **arguments)
        if queued["state"] in {"queued", "retry"}:
            self.cli("worker", "--once")
        status = self.call("workflow.status", job_id=queued["id"])
        # One job runs per repository at a time. A server this test started may
        # still hold that slot (the sync's projection takes about a second in a
        # debug build), so wait for it instead of racing it.
        deadline = time.monotonic() + 60
        while status["state"] in {"queued", "running"} and time.monotonic() < deadline:
            time.sleep(0.2)
            if status["state"] == "queued":
                self.cli("worker", "--once")
            status = self.call("workflow.status", job_id=queued["id"])
        return status

    def connect(self, kind, **config):
        config.setdefault("acl", {"workspace_readers": True, "credentials": []})
        return self.call("connector.add", name=f"Fixture {kind}", kind=kind, config=config,
                         credential_env="GRAPHICLE_FIXTURE_CREDENTIAL")["id"]

    def read(self, identifier):
        return self.call("read", source_id=identifier, line_count=400)["quote"]

    def start(self, *options):
        self.process = subprocess.Popen(self.command_line("serve", "--port", "0", *options),
                                        env=self.env, stdout=subprocess.PIPE,
                                        stderr=subprocess.PIPE, text=True)
        line = self.process.stderr.readline()
        assert "Graphicle is ready at http://" in line, line
        self.url = line.strip().split(" at ")[1]
        return self.url

    def http(self, path, body=None, headers=None):
        request = Request(self.url + path, data=None if body is None else json.dumps(body).encode(),
                          headers={"Content-Type": "application/json",
                                   "X-Workspace-Capability": self.session["capability"], **(headers or {})})
        try:
            with urlopen(request, timeout=10) as response:
                data = response.read()
                return response.status, json.loads(data) if data else None, dict(response.headers)
        except HTTPError as error:
            return error.code, json.loads(error.read()), dict(error.headers)

    def close(self):
        if self.process:
            self.process.send_signal(signal.SIGINT)
            self.process.communicate(timeout=10)
            assert self.process.returncode == 0


@pytest.fixture
def service(tmp_path):
    value = Service(tmp_path)
    try:
        yield value
    finally:
        value.close()


@contextmanager
def remote(responder):
    requests = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_GET(self):
            self.respond()

        def do_POST(self):
            self.respond()

        def respond(self):
            parsed = urlparse(self.path)
            payload = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))) or b"null")
            request = {"method": self.command, "path": parsed.path,
                       "query": parse_qs(parsed.query), "body": payload, "headers": {k.lower(): v for k, v in self.headers.items()}}
            requests.append(request)
            try:
                value = responder(request)
                status, value = value if isinstance(value, tuple) else (200, value)
            except Exception as error:
                status, value = 500, {"fixture_error": str(error)}
            raw = value.encode() if isinstance(value, str) else json.dumps(value).encode()
            self.send_response(status)
            self.send_header("Content-Type", "text/plain" if isinstance(value, str) else "application/json")
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", requests
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def endpoint(service, kind, address):
    service.config.write_text(json.dumps({"connector_endpoints": {kind: address}}))


def test_github_pagination_comments_restart_and_exact_file_text(service):
    source = "def café():\n    return 'north entrance'\n"

    def answer(r):
        if r["path"] == "/repos/workshop/manual/issues":
            if r["query"]["page"] == ["1"]:
                return [{"number": n, "title": f"Issue {n}", "body": f"Original issue {n}.",
                         "user": {"login": "mira"}, "comments": int(n == 1)} for n in range(1, 101)]
            return []
        if r["path"].endswith("/issues/1/comments"):
            return [{"body": "The current entrance is north.", "user": {"login": "mira"}}]
        if r["path"].endswith("/git/trees/main"):
            return {"sha": "commit", "truncated": False, "tree": [
                {"path": "main.py", "type": "blob", "sha": "blob", "size": len(source.encode())},
                {"path": ".env", "type": "blob", "sha": "secret", "size": 30}]}
        if r["path"].endswith("/git/blobs/blob"):
            return {"encoding": "base64", "content": base64.b64encode(source.encode()).decode()}
        raise AssertionError(r["path"])

    with remote(answer) as (address, requests):
        endpoint(service, "github", address)
        connector = service.connect("github", repository="workshop/manual", branch="main")
        first = service.job("connector", connector_id=connector, max_requests=1)
        assert first["state"] == "complete", first
        assert first["result"]["complete"] is False
        assert service.call("files")["total"] == 1  # old source view survives partial scan
        second = service.job("connector", connector_id=connector, max_requests=8)
        assert second["state"] == "complete", second
        assert second["result"]["complete"] is True
        assert service.call("files")["total"] == 102
        assert service.read(f"connector:{connector}:workshop/manual:file:main.py") == source
        assert "current entrance is north" in service.read(f"connector:{connector}:workshop/manual:issue:1")
        assert any(r["query"].get("page") == ["2"] for r in requests)
        assert not any(r["path"].endswith("/secret") for r in requests)


def test_notion_preserves_nested_block_order_and_pagination(service):
    def block(identifier, text, children=False):
        return {"id": identifier, "type": "paragraph", "has_children": children,
                "paragraph": {"rich_text": [{"plain_text": text}]}}

    def answer(r):
        if r["path"] == "/pages/page":
            return {"properties": {"Name": {"type": "title", "title": [{"plain_text": "Runbook"}]}}}
        if r["path"] == "/blocks/page/children":
            if r["query"].get("start_cursor") == ["next"]:
                return {"results": [block("c", "Last sibling")], "has_more": False}
            return {"results": [block("a", "Parent", True), block("b", "Next sibling")],
                    "has_more": True, "next_cursor": "next"}
        if r["path"] == "/blocks/a/children":
            return {"results": [block("child", "Nested condition")], "has_more": False}
        raise AssertionError(r["path"])

    with remote(answer) as (address, requests):
        endpoint(service, "notion", address)
        connector = service.connect("notion", page_ids=["page"])
        job = service.job("connector", connector_id=connector, max_requests=10)
        assert job["state"] == "complete", job
        text = service.read(f"connector:{connector}:page")
        assert text.index("Parent") < text.index("Nested condition") < text.index("Next sibling") < text.index("Last sibling")
        assert all(r["headers"].get("notion-version") == "2025-09-03" for r in requests)


def test_slack_fetches_full_threads_with_attribution(service):
    def answer(r):
        if r["path"] == "/conversations.history":
            return {"ok": True, "messages": [{"ts": "100.1", "user": "U1", "text": "Use south", "reply_count": 2}],
                    "response_metadata": {"next_cursor": ""}}
        if r["path"] == "/conversations.replies":
            if r["query"].get("cursor") == ["next"]:
                return {"ok": True, "messages": [{"ts": "102.1", "user": "U2", "text": "Correction: use north"}],
                        "response_metadata": {"next_cursor": ""}}
            return {"ok": True, "messages": [{"ts": "100.1", "user": "U1", "text": "Use south"}],
                    "response_metadata": {"next_cursor": "next"}}
        raise AssertionError(r["path"])

    with remote(answer) as (address, _):
        endpoint(service, "slack", address)
        connector = service.connect("slack", channels=["C1"])
        job = service.job("connector", connector_id=connector, max_requests=10)
        assert job["state"] == "complete", job
        text = service.read(f"connector:{connector}:C1:100.1")
        assert "U1" in text and "U2" in text and "1970-01-01T00:01:42" in text and "Correction: use north" in text


def test_drive_exports_docs_updates_removes_and_keeps_unchanged_source_state(service):
    state = {"text": "Entrance is north.", "deleted": False}

    def answer(r):
        if r["path"] == "/files/doc":
            return {"id": "doc", "name": "Workshop", "mimeType": "application/vnd.google-apps.document",
                    "modifiedTime": "2026-09-22T10:00:00Z", "trashed": state["deleted"]}
        if r["path"] == "/files/doc/export":
            return state["text"]
        raise AssertionError(r["path"])

    with remote(answer) as (address, _):
        endpoint(service, "gdrive", address)
        connector = service.connect("gdrive", file_ids=["doc"])
        assert service.job("connector", connector_id=connector)["state"] == "complete"
        before = service.cli("status")["source_state"]
        assert service.job("connector", connector_id=connector)["state"] == "complete"
        assert service.cli("status")["source_state"] == before
        state["text"] = "Entrance is east."
        assert service.job("connector", connector_id=connector)["state"] == "complete"
        assert service.read(f"connector:{connector}:doc") == state["text"]
        state["deleted"] = True
        assert service.job("connector", connector_id=connector)["state"] == "complete"
        assert service.call("files")["total"] == 1


@pytest.mark.parametrize("protocol", ["responses", "chat", "ollama"])
def test_each_model_protocol_runs_native_answering_and_records_usage(service, protocol):
    answer = {"action": "answer", "answer": "Mira prefers quiet rooms.", "query": None, "queries": [], "reads": [],
              "source_id": None, "start_byte": None, "evidence": [{"id": "file:notes.txt", "quote": "Mira prefers quiet rooms."}], "unresolved": []}

    def respond(r):
        if protocol == "responses":
            assert r["path"] == "/responses" and r["body"]["store"] is False
            return {"status": "completed", "usage": {"input_tokens": 100, "output_tokens": 30},
                    "output": [{"content": [{"type": "output_text", "text": json.dumps(answer)}]}]}
        if protocol == "chat":
            assert r["path"] == "/chat/completions"
            assert r["body"]["response_format"]["json_schema"]["strict"] is True
            return {"choices": [{"finish_reason": "stop", "message": {"content": json.dumps(answer)}}],
                    "usage": {"prompt_tokens": 100, "completion_tokens": 30}}
        assert r["path"] == "/api/chat" and r["body"]["stream"] is False
        return {"done": True, "done_reason": "stop", "message": {"content": json.dumps(answer)},
                "prompt_eval_count": 100, "eval_count": 30}

    with remote(respond) as (address, requests):
        service.config.write_text(json.dumps({"default_model": "test", "models": {"test": {
            "protocol": protocol, "base_url": address, "model": "fixture", "input_limit": 16000, "output_limit": 2048}}}))
        job = service.job("reflect", question="Which rooms does Mira prefer?", max_calls=1)
        assert job["state"] == "complete", job
        assert job["result"]["answer"] == answer["answer"]
        assert len(requests) == 1
        assert job["calls"][0]["usage_reported"] == 1
        assert job["calls"][0]["input_tokens"] == 100


def test_http_host_origin_scope_and_real_mcp_stdio_and_http(service):
    service.start()
    assert service.http("/health")[1]["runtime"] == "rust"
    assert service.http("/health", headers={"Host": "evil.example"})[0] == 403
    assert service.http("/health", headers={"Origin": "https://evil.example"})[0] == 403
    path = f"/api/workspaces/{service.session['workspace']}/repositories/command"
    code, _, headers = service.http(path, {"operation": "context", "arguments": {
        "repository_id": service.session["project"], "question": "quiet rooms"}})
    assert code == 200 and headers["cache-control"] == "no-store"
    assert service.http(path, {"operation": "context", "arguments": {"repository_id": "another-workspace", "question": "quiet"}})[0] == 400
    assert service.http("/api/local-session", {"ticket": "wrong-ticket"})[0] == 403
    browser_path = f"/api/workspaces/{service.session['workspace']}/browser-session"
    assert service.http(browser_path, {}, {"X-Workspace-Capability": "wrong-key"})[0] == 403
    assert service.http(browser_path, {"project": "another-workspace"})[0] == 403
    code, issued, _ = service.http(browser_path, {"project": service.session["project"]})
    assert code == 200 and issued["expires_in_seconds"] == 180
    code, opened, _ = service.http("/api/local-session", {"ticket": issued["ticket"]})
    assert code == 200 and opened["workspace"] == service.session["workspace"]
    assert service.http("/api/local-session", {"ticket": issued["ticket"]})[0] == 403

    async def check():
        import httpx
        from mcp import ClientSession, StdioServerParameters
        from mcp.client.stdio import stdio_client
        from mcp.client.streamable_http import streamable_http_client

        async def inspect(read, write):
            async with ClientSession(read, write, read_timeout_seconds=15) as session:
                await session.initialize()
                names = {t.name for t in (await session.list_tools()).tools}
                assert "recall_project" in names and "prepare_observations" in names
                assert not any("approve" in name for name in names)
                result = await session.call_tool("recall_project", {"question": "quiet rooms"})
                assert not result.is_error
                assert "quiet rooms" in result.structured_content["markdown"]
                assert result.structured_content["model_calls"] == 0

        parameters = StdioServerParameters(command=str(BINARY), args=service.command_line()[1:] + ["mcp"], env=service.env)
        async with asyncio.timeout(40):
            async with stdio_client(parameters) as (read, write):
                await inspect(read, write)
            url = f"{service.url}/mcp/{service.session['workspace']}/{service.session['project']}"
            async with httpx.AsyncClient(headers={"Authorization": f"Bearer {service.session['capability']}"}) as client:
                async with streamable_http_client(url, http_client=client) as (read, write):
                    await inspect(read, write)

    asyncio.run(check())


def test_connect_graph_serves_one_server_through_the_official_mcp_client(tmp_path):
    """The one MCP server through the official MCP client.

    A fresh store, `sync .`, `connect claude-code --graph --tools all` (the
    whole server, as 0.1.0 connected Claude Code), then one stdio session of
    the entry's own command. `initialize` and `tools/list` equal the frozen
    list (tests/fixtures/one_server_tools.json, the Core server: regenerate it
    when a change alters the list). query_graph,
    recall_project and propose_memory answer in
    that one connection, and query_graph and recall_project cite one
    stable_id for the function.
    """
    data, folder = tmp_path / "store", tmp_path / "billing"
    folder.mkdir()
    (folder / "invoice.py").write_text(
        "from money import quantize_cents\n\n\ndef settle_invoice(total, tax):\n"
        "    \"\"\"Settle an invoice: the amount due with tax.\"\"\"\n"
        "    return quantize_cents(total + tax)\n")
    (folder / "money.py").write_text(
        "def quantize_cents(value):\n    scaled = int(value * 100 + 0.5)\n    return scaled / 100\n")
    env = dict(os.environ, HOME=str(tmp_path))

    def cli(*arguments):
        result = subprocess.run([str(BINARY), "--data-dir", str(data), *arguments], cwd=folder,
                                env=env, text=True, capture_output=True, timeout=120)
        assert result.returncode == 0, result.stderr
        return json.loads(result.stdout)

    assert cli("sync", ".")["graph"]["status"] == "built"
    assert cli("connect", "claude-code", "--graph", "--tools", "all")["entry"] == "graphicle"
    servers = json.loads((folder / ".mcp.json").read_text())["mcpServers"]
    assert list(servers) == ["graphicle"], servers
    entry = servers["graphicle"]
    frozen = json.loads((ROOT / "tests/fixtures/one_server_tools.json").read_text())

    def plain(model):
        return model.model_dump(by_alias=True, mode="json", exclude_none=True)

    async def check():
        from mcp import ClientSession, StdioServerParameters
        from mcp.client.stdio import stdio_client

        parameters = StdioServerParameters(command=entry["command"], args=entry["args"], env=env,
                                           cwd=str(folder))
        async with stdio_client(parameters) as (read, write):
            async with ClientSession(read, write, read_timeout_seconds=60) as session:
                assert plain(await session.initialize())["instructions"] == frozen["instructions"]
                listed = [plain(tool) for tool in (await session.list_tools()).tools]
                assert [t["name"] for t in listed] == [t["name"] for t in frozen["tools"]]
                for tool, expected in zip(listed, frozen["tools"]):
                    for key in ("description", "inputSchema", "annotations"):
                        assert tool.get(key) == expected.get(key), (tool["name"], key)

                async def call(name, arguments):
                    # The whole packet in structuredContent (the default reply is the text).
                    if name in ("query_graph", "recall_project"):
                        arguments = {**arguments, "structured": True}
                    result = plain(await session.call_tool(name, arguments))
                    assert not result.get("isError"), result
                    return result

                graph = await call("query_graph", {"question": "settle_invoice quantize_cents"})
                assert graph["content"][1]["text"].startswith("Citations (code-graph episode ")
                cited = next(node for node in graph["structuredContent"]["nodes"]
                             if node["label"] == "quantize_cents()")
                assert cited["checked"] and cited["source_id"] == "file:money.py", cited
                recall = await call("recall_project", {"question": "Which helper does settle_invoice depend on?"})
                facts = [window["code_graph"] for window in recall["structuredContent"]["evidence"]
                         if "code_graph" in window]
                same = next(fact for fact in facts if fact["label"] == "quantize_cents()")
                assert (same["stable_id"], same["revision"]) == (cited["stable_id"], cited["revision"])
                proposed = await call("propose_memory", {
                    "kind": "decision", "title": "Cents are rounded half up",
                    "body": "quantize_cents rounds half up to whole cents.",
                    "evidence": [{"source_id": "file:money.py", "quote": "scaled = int(value * 100 + 0.5)"}],
                    "idempotency_key": "o1-1"})
                assert proposed["structuredContent"]["state"] == "draft", proposed

    async def bounded():
        async with asyncio.timeout(120):
            await check()

    asyncio.run(bounded())


def test_public_maintenance_commands_are_wired_and_preserve_source_scope(service):
    # CLI and HTTP share api::command. These must reach the store methods,
    # not just pass tests that call the methods directly.
    connector = service.connect("github", repository="fixture/project")
    scheduled = service.call("connector.schedule", connector_id=connector, sync_every_seconds=900)
    assert scheduled["sync_every_seconds"] == 900
    assert service.call("connector.state", connector_id=connector, state="paused")["state"] == "paused"
    assert service.call("connector.list")["connectors"][0]["state"] == "paused"
    assert service.call("connector.state", connector_id=connector, state="active")["state"] == "active"
    assert service.call("connector.delete", connector_id=connector)["deleted"]
    assert not service.call("connector.list")["connectors"]
    assert "quiet rooms" in service.read("file:notes.txt")
    assert service.call("storage.prune", keep_snapshots=1)["memory_revisions_preserved"]
    snapshot = service.cli("status")["repository"]["snapshot"]
    assert service.call("project.delete", expected_snapshot=snapshot)["deleted"]
