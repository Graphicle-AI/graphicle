#!/usr/bin/env python3
"""Seed and check an isolated product workspace. Optional loopback browser preview.

No model calls, real repositories, provider accounts or customer data are used.
--binary (default: $GRAPHICLE_BIN, else this crate's debug build) selects the
program. The change-impact check runs when the program's edition includes it
(build.info change_impact); Core skips it and says so.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import sys
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT.parents[1] / "sdk/python"))
from graphicle_sdk import LocalServer, GraphicleError

FILES = {
    "app/auth.py": '''def verify_token(token):
    return token.signature_valid

def authorize(user, token):
    return user.active and verify_token(token)
''',
    "app/orders.py": '''from app.auth import authorize
from app.payments import charge

def place_order(user, token, basket):
    if not authorize(user, token):
        raise PermissionError("Access denied")
    return charge(user, basket.total)

def cancel_order(user, order):
    return order.owner_id == user.id
''',
    "app/payments.py": '''def validate_amount(amount):
    return amount > 0

def charge(user, amount):
    if not validate_amount(amount):
        raise ValueError("Invalid amount")
    return {"customer": user.id, "amount": amount, "status": "queued"}

def receipt(payment):
    return {"status": payment["status"], "amount": payment["amount"]}
''',
    "app/routes.py": '''from app.orders import place_order, cancel_order
from app.payments import receipt

def checkout(request):
    payment = place_order(request.user, request.token, request.basket)
    return receipt(payment)

def cancel(request):
    return cancel_order(request.user, request.order)
''',
    "tests/test_auth.py": '''from app.auth import verify_token, authorize

def test_signature(token):
    assert verify_token(token)

def test_inactive_account(user, token):
    user.active = False
    assert not authorize(user, token)
''',
    "tests/test_orders.py": '''from app.orders import place_order

def test_checkout_requires_access(user, token, basket):
    assert place_order(user, token, basket)["status"] == "queued"
''',
    "docs/decisions.md": """# Payment and identity decisions

Checkout must validate the user's token before a payment is queued.
Expired tokens are rejected even when their signature is valid.
Amounts must be positive. Payment providers run outside this demonstration.
The owner reviews a memory after its supporting source changes.
"""
}

def sync(project, files, commit):
    rows = [{"path": path, "sha256": hashlib.sha256(text.encode()).hexdigest(), "bytes": len(text.encode())}
            for path, text in files.items()]
    begin = project.command("sync.prepare", expected_snapshot=project.status()["repository"]["snapshot"],
                            metadata={"commit": commit}, manifest=rows)
    project.command("sync.upload", sync_id=begin["sync_id"],
                    files=[{"path": path, "content": text} for path, text in files.items()])
    project.command("sync.commit", sync_id=begin["sync_id"])
    return project.project_graph()

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path(os.environ.get("GRAPHICLE_BIN", ROOT / "target/debug/graphicle")))
    parser.add_argument("--directory", type=Path)
    parser.add_argument("--serve", action="store_true", help="Keep a loopback-only preview running until interrupted")
    args = parser.parse_args()
    binary = args.binary.resolve()
    if not binary.is_file():
        parser.error("Build with cargo build --locked from crates/graphicle/ first.")
    directory = args.directory.resolve() if args.directory else Path(tempfile.mkdtemp(prefix="graphicle-product-"))
    directory.mkdir(parents=True, exist_ok=True, mode=0o700)
    if (directory / "data").exists():
        parser.error("Choose a fresh directory; this demonstration never overwrites an existing workspace.")
    snapshot = directory / "graphicle-demo"
    shutil.copy2(binary, snapshot)
    config = directory / "models.json"
    config.write_text(json.dumps({"models": {}}))
    config.chmod(0o600)
    # A local fixture never inherits merchant credentials/configuration.
    os.environ.pop("GRAPHICLE_BILLING_CONFIG", None)
    stop = threading.Event()
    signal.signal(signal.SIGTERM, lambda *_: stop.set())
    signal.signal(signal.SIGINT, lambda *_: stop.set())
    started = time.monotonic()
    try:
        with LocalServer(snapshot, directory / "data", config, name="Graphicle product demo") as client:
            project = client.project("atlas-commerce-demo", "Atlas commerce · demo")
            base = sync(project, FILES, "a" * 40)
            newer = dict(FILES)
            newer["app/auth.py"] = newer["app/auth.py"].replace("return token.signature_valid", "return token.signature_valid and not token.expired")
            head = sync(project, newer, "b" * 40)
            project.command("note", title="Checkout keeps authorization before charging",
                            body="Demo decision: validate identity before queueing payment; reject expired tokens. Review changes against app/auth.py and app/orders.py.",
                            maintainer="Demo maintainer", kind="decision", dependencies=[])
            packet = project.context("How does checkout reject expired tokens?", max_tokens=1800)
            assert packet["evidence"], "The context response must include original evidence"
            info = client.command("build.info")
            if info["change_impact"]:
                report = project.impact(base["episode"], head["episode"], budget_tokens=6000)
                assert report["changed_nodes"], "The change report must identify modified code"
            original = project.graph_source(base["episode"], "app/auth.py")
            assert "not token.expired" not in original["quote"], "Historical reads must retain the original"
            graph = project.graph(limit=80, edge_limit=120)
            assert graph["nodes"] and graph["edges"]
            cap = client._capability  # only written to the private local fixture session
            endpoint = "/mcp/" + client.workspace + "/" + project.id
            for method, params in [
                ("initialize", {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "product-demo-check", "version": "1"}}),
                ("tools/list", {}),
                ("tools/call", {"name": "repository_status", "arguments": {}})
            ]:
                result = client.request("POST", endpoint, {"jsonrpc": "2.0", "id": method, "method": method, "params": params})
                assert "error" not in result and not result.get("result", {}).get("isError"), result
            client.set_budget(client.usage()["requests"])
            try:
                project.context("This request must hit the cap")
                raise AssertionError("Budget should reject additional retrieval")
            except GraphicleError as error:
                assert error.status == 429
            assert project.export_memories()["memories"]
            client.set_budget(None)
            session = directory / "demo-session.json"
            session.write_text(json.dumps({"url": client.url, "workspace": client.workspace, "capability": cap, "project": project.id}))
            session.chmod(0o600)
            receipt = {
                "url": client.url, "workspace": client.workspace, "project": project.id,
                "fixture_only": True, "model_calls": client.usage()["model_usage"]["calls"], "edition": info["edition"],
                "checks": ["source_sync", "graph_projection", "historical_citation", *(["change_impact"] if info["change_impact"] else []),
                           "evidence_retrieval", "mcp_initialize", "mcp_discovery",
                           "mcp_project_access", "quota_enforcement", "export_at_limit", "budget_recovery"],
                "skipped": [] if info["change_impact"] else ["change_impact: not part of this edition (build.info)"],
                "graph_nodes": len(graph["nodes"]), "graph_edges": len(graph["edges"]),
                "elapsed_seconds": round(time.monotonic() - started, 3),
                "preview_running": args.serve, "session_file": str(session)
            }
            (directory / "product-receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
            print(json.dumps(receipt, indent=2), flush=True)
            if args.serve:
                while not stop.wait(1):
                    pass
    finally:
        snapshot.unlink(missing_ok=True)

if __name__ == "__main__":
    main()
