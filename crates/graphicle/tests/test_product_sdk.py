"""Product SDK workflow over a real local service, with no model or cloud calls.

GRAPHICLE_BIN selects the program (default: this crate's debug build, Graphicle
Core). Change impact follows the edition `build.info` reports: Core answers 501,
and an edition's tests run the impact report through its own program.
"""
import hashlib
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT.parents[1] / "sdk/python"))
from graphicle_sdk import Client, LocalServer, GraphicleError


@pytest.fixture
def product(tmp_path, monkeypatch):
    monkeypatch.delenv("GRAPHICLE_BILLING_CONFIG", raising=False)
    source = Path(os.environ.get("GRAPHICLE_BIN", ROOT / "target/debug/graphicle"))
    assert source.is_file(), "Build from crates/graphicle/ first (its .cargo config bundles parsers)"
    binary = tmp_path / "graphicle"
    shutil.copy2(source, binary)
    # The shared target can be rebuilt by another process; keep a private executable.
    try:
        with LocalServer(binary, tmp_path / "data") as client:
            yield client, client.project("product-sdk", "Product SDK")
    finally:
        binary.unlink(missing_ok=True)


def sync(project, content):
    begin = project.command("sync.prepare", expected_snapshot=project.status()["repository"]["snapshot"],
                            manifest=[{"path": "auth.py", "sha256": hashlib.sha256(content.encode()).hexdigest(), "bytes": len(content.encode())}])
    project.command("sync.upload", sync_id=begin["sync_id"], files=[{"path": "auth.py", "content": content}])
    project.command("sync.commit", sync_id=begin["sync_id"])
    return project.project_graph()


def test_python_client_runs_the_product_workflow_and_budget_recovery(product):
    client, project = product
    first = sync(project, "def auth():\n    return 'before'\n")
    sync(project, "def auth():\n    return 'after'\n")
    assert project.overview()["graph"]["current_recorded"] is True
    assert len(project.episodes()["episodes"]) == 2
    assert "before" in project.graph_source(first["episode"], "auth.py")["quote"]
    assert project.graph(limit=10, edge_limit=10)["nodes"]
    latest = project.episodes()["current_episode"]
    if client.command("build.info")["change_impact"]:
        impact = project.impact(first["episode"], latest, budget_tokens=4000)
        assert impact["changed_nodes"]
    else:
        with pytest.raises(GraphicleError) as missing:
            project.impact(first["episode"], latest, budget_tokens=4000)
        assert missing.value.status == 501
    packet = project.context("What does auth return?")
    assert packet["evidence"] and "request_hash" not in packet
    assert project.export_memories()["format"] == "graphicle.memories.v1"
    client.set_budget(client.usage()["requests"])
    with pytest.raises(GraphicleError) as error:
        project.context("What does auth return?")
    assert error.value.status == 429
    assert project.export_memories()["format"] == "graphicle.memories.v1"
    client.set_budget(None)
    assert project.context("What does auth return?")["evidence"]


def test_python_budget_validation_does_not_send_a_request():
    client = Client("https://example.invalid", "workspace", "fixture")
    for value in [-1, 1.2, True, "100", 1_000_000_001]:
        with pytest.raises(ValueError):
            client.set_budget(value)
