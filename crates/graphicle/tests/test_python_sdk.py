"""The Python SDK (sdk/python/graphicle_sdk) against the real service over HTTP.

A private copy of the binary serves each store, and the deterministic local stub of
tools/certify_forgetting.py stands in for the answer and observer models; no request leaves
127.0.0.1. This qualifies the SDK's contract with the service (isolation, grounding, certified
forgetting, errors), not answer quality.
"""
import asyncio
import http.server
import json
import os
import shutil
import subprocess
import sys
import threading
import time
import uuid
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
SOURCE_BINARY = Path(os.environ.get("GRAPHICLE_BIN", ROOT / "target/debug/graphicle"))
sys.path.insert(0, str(ROOT.parents[1] / "sdk" / "python"))

from graphicle_sdk import (  # noqa: E402
    GraphicleError,
    GraphiclePermissionError,
    AsyncClient,
    AsyncMemory,
    AsyncProject,
    AsyncTools,
    Client,
    LocalServer,
    Memory,
    Tools,
    render_messages,
    tool_schemas,
)

pytestmark = pytest.mark.skipif(not SOURCE_BINARY.exists(), reason="build the graphicle binary first")


def load_stub():
    tools = str(ROOT / "tools")
    sys.path.insert(0, tools)  # tools/ is a folder of scripts, not a package
    try:
        from certify_forgetting import Stub
    finally:
        sys.path.remove(tools)
    return Stub


@pytest.fixture(scope="session", autouse=True)
def binary(tmp_path_factory):
    """Copied once at session start: another process may rebuild target/debug meanwhile."""
    copy = tmp_path_factory.mktemp("bin") / "graphicle"
    problem = ""
    for _ in range(5):
        try:
            shutil.copy2(SOURCE_BINARY, copy)
            check = subprocess.run([str(copy), "--version"], capture_output=True, text=True, timeout=60)
            if check.returncode == 0 and check.stdout.startswith("graphicle "):
                break
            problem = check.stderr[-500:]
        except OSError as error:  # the linker may be replacing the file right now
            problem = str(error)
        time.sleep(3)  # caught mid-rebuild; copy again
    else:
        pytest.fail(f"No runnable graphicle binary at {SOURCE_BINARY}: {problem}")
    try:
        yield copy
    finally:
        copy.unlink(missing_ok=True)  # a debug build is hundreds of MB; pytest keeps old temp dirs


@pytest.fixture(scope="session")
def models(tmp_path_factory):
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), load_stub())
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    base = f"http://127.0.0.1:{server.server_address[1]}/v1"
    path = tmp_path_factory.mktemp("models") / "models.json"
    path.write_text(json.dumps({"default_model": "stub", "embedding_model": "embed", "models": {
        "stub": {"protocol": "responses", "base_url": base, "model": "stub", "input_limit": 32000, "output_limit": 4000},
        "embed": {"protocol": "chat", "base_url": base, "model": "embed", "input_limit": 8000, "output_limit": 64}}}))
    try:
        yield path
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


@pytest.fixture(scope="module")
def local(binary, models, tmp_path_factory):
    server = LocalServer(binary, tmp_path_factory.mktemp("store") / "data", models)
    server.start()
    try:
        yield server
    finally:
        code = server.stop()
        assert code == 0, server.logs()


@pytest.fixture
def memory(local):
    # A namespace per test keeps the projects of tests sharing one server apart.
    return Memory(local.client, namespace=f"t{uuid.uuid4().hex[:10]}")


BIKE = [{"role": "user", "content": "My new bike is teal with a brass bell."},
        {"role": "assistant", "content": "Noted, a teal bike."}]
CAT = [{"role": "user", "content": "I adopted a grey cat named Pixel."},
       {"role": "assistant", "content": "Pixel sounds lovely."}]


def texts(found):
    return [hit["memory"] for hit in found["results"]]


def test_local_server_gives_a_working_client_and_keeps_the_workspace(binary, models, tmp_path):
    server = LocalServer(binary, tmp_path / "data", models)
    with server as client:
        assert isinstance(client, Client)
        assert client.url.startswith("http://127.0.0.1:") and client.health()["status"] == "ok"
        assert "capability" not in repr(client) and client._capability not in repr(client)
        project = client.project("sdk-notes")
        assert client.project("sdk-notes").id == project.id  # registered once, then reused
        assert client.find_project("sdk-notes").id == project.id and client.find_project("absent") is None
        connector = project.connector("docs")
        # A fresh Project object finds the connector by name instead of adding another one.
        assert client.project("sdk-notes").connector("docs") == connector
        assert [c["name"] for c in project.connectors()] == ["docs"]
        long_text = "".join(f"line {i}: filler about the weather.\n" for i in range(900)) + "The key is under the blue pot.\n"
        imported = project.import_documents(connector, [
            {"id": "n1", "title": "Door", "content": "The spare key hangs by the north door.\n"},
            {"id": "n2", "content": long_text}])
        assert imported["imported"] == 2 and imported["forgotten_skipped"] == 0
        packet = project.context("Where is the spare key?", max_tokens=2000)
        assert packet["model_calls"] == 0
        hits = {e["id"]: e for e in packet["evidence"]}
        assert hits[f"connector:{connector}:n1"]["text"] == "The spare key hangs by the north door.\n"
        # Reads longer than one window are joined into the whole original.
        document = project.read_document(f"connector:{connector}:n2")
        assert document["content"] == long_text and document["title"] == "n2"
        assert project.status()["evidence_sources"] == 2
        workspace = client.workspace
    assert server.process is None
    with pytest.raises(GraphicleError) as stopped:
        client.health()
    assert stopped.value.status is None  # unreachable, not an HTTP error
    # init reuses the saved session: the same workspace and data come back.
    with LocalServer(binary, tmp_path / "data", models) as again:
        assert again.workspace == workspace
        assert again.find_project("sdk-notes").id == project.id


def test_add_and_search_keep_users_apart(memory):
    alice = memory.add(BIKE, user_id="alice", timestamp="2024-05-01", metadata={"topic": "bike"})
    bob = memory.add(CAT, user_id="bob")
    [added] = alice["results"]
    stored = render_messages(BIKE, timestamp="2024-05-01")
    assert stored.startswith("Conversation date: 2024-05-01\n\nuser: My new bike is teal")
    assert added["event"] == "ADD" and added["memory"] == stored and added["user_id"] == "alice"
    project_a, project_b = memory.project_for(user_id="alice"), memory.project_for(user_id="bob")
    assert project_a.id != project_b.id and added["id"].startswith(project_a.id + ".")

    found = memory.search("What colour is the bike with the brass bell?", filters={"user_id": "alice"})
    assert found["results"], found
    first = found["results"][0]
    assert first["id"] == added["id"] and first["score"] is None and first["rank"] == 1
    assert first["memory"] in stored and "teal" in first["memory"]  # an exact excerpt of the original
    assert stored.encode()[first["byte_start"]:first["byte_end"]].decode() == first["memory"]
    assert first["metadata"] == {"topic": "bike"} and first["user_id"] == "alice"

    # Bob's scope never returns Alice's text, whatever he asks and in either argument form.
    for query in ["What colour is the bike with the brass bell?", "teal bike brass bell", "Noted"]:
        for result in [memory.search(query, filters={"user_id": "bob"}), memory.search(query, user_id="bob", limit=40)]:
            assert not any("teal" in text or "brass" in text for text in texts(result)), result
    assert "Pixel" in texts(memory.search("grey cat", filters={"user_id": "bob"}))[0]
    assert not any("Pixel" in text for text in texts(memory.search("grey cat", filters={"user_id": "alice"})))
    everything_b = memory.get_all(filters={"user_id": "bob"})
    assert [r["id"] for r in everything_b["results"]] == [bob["results"][0]["id"]]
    assert not any("teal" in text for text in texts(everything_b))
    assert not any("teal" in e["text"] for e in project_b.context("teal bike brass bell")["evidence"])
    assert memory.search("bike", filters={"user_id": "nobody"}) == {"results": []}

    # The same session again is not a duplicate; other metadata updates the stored version.
    again = memory.add(BIKE, user_id="alice", timestamp="2024-05-01", metadata={"topic": "bike"})
    assert again["results"][0]["event"] == "NONE" and again["results"][0]["id"] == added["id"]
    relabelled = memory.add(BIKE, user_id="alice", timestamp="2024-05-01", metadata={"topic": "cycling"})
    assert relabelled["results"][0]["event"] == "UPDATE" and relabelled["results"][0]["id"] == added["id"]
    assert len(memory.get_all(filters={"user_id": "alice"})["results"]) == 1
    fetched = memory.get(added["id"])
    assert fetched["memory"] == stored and fetched["metadata"] == {"topic": "cycling"}
    assert fetched["created_at"] and fetched["updated_at"]


def test_run_and_metadata_filters_narrow_inside_a_user(memory):
    one = memory.add("I prefer window seats on trains.", user_id="gia", run_id="r1", metadata={"kind": "travel"})
    memory.add("I prefer aisle seats on planes.", user_id="gia", run_id="r2", metadata={"kind": "travel"})
    in_r1 = memory.search("seats prefer", filters={"user_id": "gia", "run_id": "r1"})
    assert [r["id"] for r in in_r1["results"]] == [one["results"][0]["id"]]
    assert len(memory.search("seats prefer", filters={"user_id": "gia", "kind": "travel"})["results"]) == 2
    assert memory.search("seats prefer", filters={"user_id": "gia", "kind": "food"})["results"] == []
    assert len(memory.get_all(user_id="gia", run_id="r2")["results"]) == 1

    memory.update(one["results"][0]["id"], metadata={"mood": "calm"})
    assert memory.get(one["results"][0]["id"])["metadata"] == {"kind": "travel", "mood": "calm"}
    memory.update(one["results"][0]["id"], text="user: I now prefer seats facing forward.\n\n")
    assert "facing forward" in texts(memory.search("facing forward", filters={"user_id": "gia"}))[0]
    assert memory.get(one["results"][0]["id"])["run_id"] == "r1"


def test_history_lists_each_stored_text_oldest_first(memory):
    [added] = memory.add("user: My locker number is 12.", user_id="hal")["results"]
    memory.update(added["id"], metadata={"place": "gym"})  # metadata only: no new version
    memory.update(added["id"], text="user: My locker number is 31.\n\n")
    history = memory.history(added["id"])
    assert [entry["event"] for entry in history] == ["ADD", "UPDATE"], history
    assert history[0]["old_memory"] is None and "is 12" in history[0]["new_memory"]
    assert "is 12" in history[1]["old_memory"] and "is 31" in history[1]["new_memory"]
    assert all(entry["memory_id"] == added["id"] and entry["created_at"] for entry in history)
    memory.delete(added["id"])
    assert memory.history(added["id"]) == []  # forgetting erases the versions too


def test_run_ids_narrow_on_the_service_before_ranking(memory):
    # Fifty distinct run-r2 sessions outrank the one r1 session for this question. Narrowing only
    # the 40 retrieved excerpts would miss it; the service narrows before ranking.
    for i in range(50):
        filler = " ".join(f"k{i}v{j}" for j in range(24))
        memory.add(f"user: Budget review {i}: {filler}. The budget review is done.",
                   user_id="ivy", run_id="r2")
    [target] = memory.add("user: Notes on the budget.", user_id="ivy", run_id="r1")["results"]
    found = memory.search("budget review", filters={"user_id": "ivy", "run_id": "r1"}, top_k=5)
    assert [hit["id"] for hit in found["results"]] == [target["id"]], found


def test_answer_through_the_stub_model_cites_stored_originals(memory):
    [added] = memory.add(BIKE, user_id="carol", timestamp="2024-05-01")["results"]
    project = memory.project_for(user_id="carol")
    connector = memory.connector_id(project)
    result = project.answer("What colour is my bike?", question_time="2024-06-01", max_calls=2)
    assert result["mode"] == "basic" and result["abstain"] is False and result["evidence"], result
    stored = memory.get(added["id"])["memory"]
    for citation in result["evidence"]:
        assert citation["id"] == f"connector:{connector}:{added['id']}"
        assert citation["quote"] and citation["quote"] in stored  # grounded in the original text
    assert "teal" in result["answer"]
    observed = project.observe(connector, max_calls=4)
    assert observed["complete"] is True and observed["human_verified"] is False
    assert project.observer_status(include=[f"connector:{connector}:"])["complete"] is True
    with_notes = project.answer("What colour is my bike?", question_time="2024-06-01",
                                observer_notes=True, max_calls=2)
    assert with_notes["context"]["observer_notes"]["citable"] is False
    assert all(c["quote"] in stored for c in with_notes["evidence"])
    v3 = project.answer("What colour is my bike?", question_time="2024-06-01",
                        answer_contract="grounded-v3", max_calls=3)
    assert v3["abstain"] is False and v3["premise_holds"] is True and v3["reading_notes"], v3
    assert v3["context"]["observer_notes"]["selected"] >= 1  # grounded-v3 reads notes by default
    assert "memory_profile" not in v3["context"]  # the reflection profile is not supported
    assert all(c["quote"] in stored for c in v3["evidence"])
    plain = project.answer("What colour is my bike?", question_time="2024-06-01",
                           answer_contract="grounded-v3", observer_notes=False, max_calls=2)
    assert "observer_notes" not in plain["context"] and "memory_profile" not in plain["context"], plain


def test_delete_forgets_with_a_verifiable_receipt_and_stays_forgotten(memory):
    secret = [{"role": "user", "content": "The shed padlock code is 4417."}]
    [forgotten] = memory.add(secret, user_id="dave")["results"]
    [kept] = memory.add([{"role": "user", "content": "I planted tomatoes on the balcony."}], user_id="dave")["results"]
    assert any("4417" in t for t in texts(memory.search("shed padlock code", filters={"user_id": "dave"})))

    deleted = memory.delete(forgotten["id"])
    receipt = deleted["receipt"]
    assert deleted["message"] == "Memory deleted successfully!"
    assert receipt["operation"] == "source.forget" and receipt["verified_local"] is True
    found = memory.search("shed padlock code", filters={"user_id": "dave"})
    assert all(r["id"] != forgotten["id"] and "4417" not in r["memory"] for r in found["results"])
    assert memory.get(forgotten["id"]) is None
    project = memory.project_for(user_id="dave")
    verified = project.verify(receipt["id"])  # deletion.verify re-checks the current store
    assert verified["verified_local"] is True and verified["complete"] is True and verified["chain"]["intact"]
    assert [r["id"] for r in project.receipts()["receipts"]] == [receipt["id"]]

    # Re-adding the same session, through the facade or as the same connector document, is skipped.
    assert memory.add(secret, user_id="dave") == {"results": [], "forgotten_skipped": 1}
    document = {"id": forgotten["id"], "title": "Conversation", "content": render_messages(secret)}
    assert project.import_documents(memory.connector_id(project), [document])["forgotten_skipped"] == 1
    assert not any("4417" in t for t in texts(memory.search("shed padlock code", filters={"user_id": "dave"})))
    assert project.verify(receipt["id"])["verified_local"] is True
    assert "tomatoes" in texts(memory.search("tomatoes balcony", filters={"user_id": "dave"}))[0]
    assert memory.get(kept["id"]) is not None
    with pytest.raises(ValueError, match="not found"):
        memory.delete(forgotten["id"])


def test_delete_all_forgets_every_session_of_the_scope(memory):
    for text in ["I run on Mondays.", "I swim on Tuesdays."]:
        memory.add(text, user_id="erin", run_id="week-1")
    memory.add("I cycle on Fridays.", user_id="erin", run_id="week-2")
    memory.add("I climb on Sundays.", user_id="frank")

    narrow = memory.delete_all(user_id="erin", run_id="week-1")
    assert narrow["deleted"] == 2 and len(narrow["receipts"]) == 1
    assert all(r["verified_local"] for r in narrow["receipts"])
    assert texts(memory.get_all(user_id="erin")) == [render_messages("I cycle on Fridays.")]

    everything = memory.delete_all(user_id="erin")
    assert everything["deleted"] == 1
    assert memory.get_all(filters={"user_id": "erin"}) == {"results": []}
    assert memory.search("Mondays Tuesdays Fridays", filters={"user_id": "erin"}) == {"results": []}
    assert texts(memory.get_all(filters={"user_id": "frank"})) == [render_messages("I climb on Sundays.")]
    project = memory.project_for(user_id="erin")
    for receipt in narrow["receipts"] + everything["receipts"]:
        assert project.verify(receipt["id"])["verified_local"] is True
    with pytest.raises(ValueError, match="At least one filter"):
        memory.delete_all()


def test_reset_deletes_only_this_namespace(local, memory):
    other = Memory(local.client, namespace=f"o{uuid.uuid4().hex[:10]}")
    other.add(CAT, user_id="hana")
    memory.add(BIKE, user_id="hana")
    memory.add(CAT, user_id="ivan")
    reset = memory.reset()
    assert len(reset["receipts"]) == 2 and all(r["verified_local"] for r in reset["receipts"])
    assert memory.get_all(user_id="hana") == {"results": []}
    assert memory.project_for(user_id="ivan") is None
    assert "Pixel" in texts(other.search("grey cat", filters={"user_id": "hana"}))[0]


def test_bad_capability_raises_permission_error(local):
    bad = Client(local.url, local.session["workspace"], "not-the-capability")
    with pytest.raises(GraphiclePermissionError) as denied:
        bad.project("anything")
    assert isinstance(denied.value, PermissionError) and isinstance(denied.value, GraphicleError)
    assert denied.value.status == 403 and denied.value.message == "Workspace access denied"
    with pytest.raises(PermissionError):
        Memory(bad).search("anything", filters={"user_id": "alice"})
    with pytest.raises(GraphicleError) as rejected:
        local.client.command("no.such.operation")
    assert rejected.value.status == 400 and not isinstance(rejected.value, PermissionError)


def test_the_capability_is_never_sent_over_plain_http_to_another_host():
    with pytest.raises(ValueError, match="loopback"):
        Client("http://memory.example.com:8854", "w", "secret")
    for url in (
        "https://memory.example.com",
        "http://127.0.0.1:1",
        "http://127.0.0.2:1",
        "http://localhost:1",
        "http://[::1]:1",
        "http://[::ffff:127.0.0.1]:1",
    ):
        Client(url, "w", "secret")
    Client("http://10.0.0.5:8854", "w", "secret", insecure_http=True)


def test_memory_facade_argument_shapes_and_unsupported_features(memory):
    with pytest.raises(ValueError, match="at least one of"):
        memory.search("anything")
    with pytest.raises(ValueError, match="at least one of"):
        memory.get_all(filters={"topic": "x"})
    with pytest.raises(ValueError):
        memory.add("hello")  # no scope
    with pytest.raises(ValueError, match="whitespace"):
        memory.add("hello", user_id="two words")
    for unsupported in [{"expiration_date": "2030-01-01"}, {"memory_type": "procedural_memory"}, {"prompt": "x"}]:
        with pytest.raises(ValueError):
            memory.add("hello", user_id="u", **unsupported)
    with pytest.raises(ValueError, match="Unsupported filter"):
        memory.search("x", filters={"user_id": "u", "score": {"gt": 1}})
    with pytest.raises(TypeError):
        memory.search("x", filters={"user_id": "u"}, colour="red")
    assert memory.history("anything") == []
    assert memory.get("not-a-memory-id") is None
    assert memory.add([{"role": "assistant", "content": None}], user_id="u") == {"results": []}
    rendered = render_messages([{"role": "system", "content": "Be brief."},
                                {"role": "user", "name": "Ana", "content": [{"type": "text", "text": "Hi"}]}],
                               timestamp="2023/05/20 (Sat) 02:21")
    assert rendered == "Conversation date: 2023/05/20 (Sat) 02:21\n\nsystem: Be brief.\n\nuser (Ana): Hi\n\n"


# Function calling and async use -------------------------------------------------------------

def shipped_project(client, key):
    project = client.project(key)
    chats = project.connector("chats")
    project.import_documents(chats, [{"id": "s1", "title": "Kickoff",
                                      "content": "user: We ship the teal release on Friday.\n\nassistant: Noted.\n\n"}])
    return project, chats


def test_tool_schemas_are_the_services_published_mcp_tools(local):
    project = local.client.project(f"tools-{uuid.uuid4().hex[:8]}")
    listed = local.client.request("POST", f"/mcp/{local.client.workspace}/{project.id}",
                                  {"jsonrpc": "2.0", "id": 1, "method": "tools/list"})["result"]["tools"]
    tools = Tools(project)
    readable = [tool for tool in listed if tool["annotations"]["readOnlyHint"]]
    assert tools.definitions() == readable and "recall_project" in tools.names()
    by_name = {tool["name"]: tool for tool in listed}
    for entry in tools.schemas("openai"):
        assert entry["type"] == "function"
        assert entry["function"]["parameters"] == by_name[entry["function"]["name"]]["inputSchema"]
    anthropic = {entry["name"]: entry for entry in tools.schemas("anthropic")}
    assert anthropic["session_timeline"]["input_schema"] == by_name["session_timeline"]["inputSchema"]
    assert [entry["name"] for entry in tools.schemas("openai-responses")] == tools.names()
    assert tool_schemas(project, "anthropic") == tools.schemas("anthropic")
    # This server runs without MCP learning, so no memory write tool is published; the one
    # write tool it publishes is graph_link, which persists cross-repository edges.
    assert "propose_memory" not in by_name and "graph_link" not in tools.names()
    assert sorted(Tools(project, include_write=True).names()) == sorted(tools.names() + ["graph_link"])
    with pytest.raises(ValueError):
        tools.schemas("gemini")


def test_tool_calls_run_through_mcp_and_failures_go_back_to_the_model(local):
    project, chats = shipped_project(local.client, f"calls-{uuid.uuid4().hex[:8]}")
    tools = Tools(project)
    chat = tools.resolve({"id": "call_1", "type": "function", "function": {
        "name": "recall_project", "arguments": json.dumps({"question": "When does the teal release ship?"})}})
    assert chat["role"] == "tool" and chat["tool_call_id"] == "call_1"
    assert "Friday" in json.loads(chat["content"])["evidence"][0]["text"]
    block = tools.resolve({"type": "tool_use", "id": "toolu_1", "name": "read_project_source",
                           "input": {"source_id": f"connector:{chats}:s1"}})
    assert block["type"] == "tool_result" and "is_error" not in block
    assert "teal release" in json.loads(block["content"])["quote"]
    item = tools.resolve({"type": "function_call", "call_id": "fc_1", "name": "repository_files", "arguments": ""})
    assert item["type"] == "function_call_output" and f"connector:{chats}:s1" in item["output"]

    unknown = tools.resolve({"type": "tool_use", "id": "toolu_2", "name": "approve_memory", "input": {}})
    assert unknown["is_error"] is True and "Unknown tool" in unknown["content"]
    invalid = tools.resolve({"type": "tool_use", "id": "toolu_3", "name": "recall_project", "input": {"question": 7}})
    assert invalid["is_error"] is True and "contract" in invalid["content"]  # the service's schema check
    with pytest.raises(ValueError, match="repository_id"):
        tools.call("recall_project", {"question": "anything", "repository_id": "another-project"})
    with pytest.raises(ValueError, match="Unknown tool"):
        tools.call("run_command", {})


def test_tools_of_one_users_project_read_that_project_only(memory):
    # One key reads every user's project, but a model given Tools(project) cannot
    # reach another project unless the application opts in with cross_project=True.
    memory.add(BIKE, user_id="alice")
    memory.add(CAT, user_id="bob")
    alice, bob = memory.project_for(user_id="alice"), memory.project_for(user_id="bob")
    tools = Tools(bob)
    assert "list_projects" not in tools.names()
    for name in ("recall_project", "read_project_source"):
        schema = next(tool for tool in tools.definitions() if tool["name"] == name)["inputSchema"]
        assert "repo" not in schema["properties"]
    for arguments in ({"question": "What color is my bike?", "repo": alice.id},
                      {"question": "What color is my bike?", "scope": "workspace"}):
        with pytest.raises(ValueError, match="this project only"):
            tools.call("recall_project", arguments)
    # Sent anyway, the service refuses both: a project connection reads its own project.
    path = f"/mcp/{bob.client.workspace}/{bob.id}"
    for arguments in ({"question": "bike", "repo": alice.id}, {"question": "bike", "scope": "workspace"}):
        reply = bob.client.request("POST", path, {"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                                                  "params": {"name": "recall_project", "arguments": arguments}})
        assert "workspace connection" in reply["error"]["message"]
    assert "teal" not in json.dumps(tools.call("recall_project", {"question": "What color is my bike?"}))
    # Opted in: the workspace connection's tools, this project first, another one by repo.
    wide = Tools(bob, cross_project=True)
    assert "list_projects" in wide.names()
    packet = wide.call("recall_project", {"question": "What color is my bike?"})
    assert packet["scope"] == "workspace" and packet["projects"][0]["repository_id"] == bob.id
    named = wide.call("recall_project", {"question": "What color is my bike?", "repo": alice.id})
    assert named["repository_id"] == alice.id and "teal" in json.dumps(named["evidence"])


def test_memory_prompt_quotes_only_the_scope_s_excerpts(memory):
    memory.add(BIKE, user_id="alice")
    prompt = memory.prompt("What color is my bike?", user_id="alice")
    assert "[1] connector:" in prompt and "teal with a brass bell" in prompt
    assert "No stored excerpt matched" in memory.prompt("What color is my bike?", user_id="bob")
    assert memory.prompt("bike", filters={"user_id": "alice"}, instructions="Use these.").startswith("Use these.")


def test_async_client_runs_the_same_calls_in_worker_threads(local):
    async def scenario():
        client = AsyncClient(local.client.url, local.client.workspace, local.client._capability)
        project = await client.project(f"async-{uuid.uuid4().hex[:8]}")
        assert isinstance(project, AsyncProject) and project.id == project.sync.id
        chats = await project.connector("chats")
        await project.import_documents(chats, [{"id": "a1", "title": "Plan",
                                                "content": "user: The amber launch moved to May.\n\n"}])
        packets = await asyncio.gather(*(project.context("When is the amber launch?") for _ in range(3)))
        assert all("May" in packet["evidence"][0]["text"] for packet in packets)
        schemas = await AsyncTools(project).schemas("anthropic")
        assert any(entry["name"] == "recall_project" for entry in schemas)
        memory = AsyncMemory(client, namespace=f"t{uuid.uuid4().hex[:10]}")
        await memory.add(CAT, user_id="carol")
        assert "Pixel" in await memory.prompt("What is my cat called?", user_id="carol")
        receipt = await project.forget(chats, ["a1"])
        assert (await project.verify(receipt["id"]))["verified_local"] is True

    asyncio.run(scenario())
