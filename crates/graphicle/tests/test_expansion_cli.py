"""Exercise the shipped CLI, process-isolated decoders and hosted boundaries."""
import base64
import hashlib
import io
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
from urllib.parse import parse_qs, urlparse
import zipfile

import pytest

from .test_transports import BINARY, service  # shared disposable service fixture


def docx(text):
    target = io.BytesIO()
    with zipfile.ZipFile(target, "w") as archive:
        archive.writestr("word/document.xml", f'<w:document xmlns:w="w"><w:body><w:p><w:r><w:t>{text}</w:t></w:r></w:p></w:body></w:document>')
    return target.getvalue()


def pdf(text):
    stream = f"BT /F1 16  Tf 72 720 Td ({text}) Tj ET".encode()
    objects = [b"<< /Type /Catalog /Pages 2 0 R >>",
               b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
               b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>",
               b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
               f"<< /Length {len(stream)} >>\nstream\n".encode() + stream + b"\nendstream"]
    result = b"%PDF-1.4\n"
    offsets = []
    for n, obj in enumerate(objects, 1):
        offsets.append(len(result))
        result += f"{n} 0 obj\n".encode() + obj + b"\nendobj\n"
    xref = len(result)
    result += b"xref\n0 6\n0000000000 65535 f \n"
    result += b"".join(f"{offset:010} 00000 n \n".encode() for offset in offsets)
    result += f"trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode()
    return result


@pytest.mark.parametrize("extension,make", [("docx", docx), ("pdf", pdf)])
def test_binary_folder_sync_retains_original_and_follows_updates(service, extension, make):
    path = service.folder / f"plan.{extension}"
    raw = make("Please use the east entrance.")
    path.write_bytes(raw)
    service.cli("sync", str(service.folder))
    assert "east entrance" in service.read(f"file:{path.name}")
    original = service.call("document.original", source_id=f"file:{path.name}")
    assert base64.b64decode(original["base64"]) == raw
    raw = make("Correction: please use the south entrance.")
    path.write_bytes(raw)
    service.cli("sync", str(service.folder))
    assert "south entrance" in service.read(f"file:{path.name}")
    assert service.call("document.original", source_id=f"file:{path.name}")["sha256"] == hashlib.sha256(raw).hexdigest()
    stale = service.root / "stale-original.json"
    stale.write_text(json.dumps({"source_id": f"file:{path.name}", "original_sha256": original["sha256"]}))
    assert "changed" in service.cli("call", "document.original", "--arguments", str(stale), ok=False)


@pytest.mark.parametrize("client,group", [("cursor", "mcpServers"), ("vscode", "servers"), ("claude-code", "mcpServers")])
def test_one_command_agent_connection_preserves_existing_configuration(service, client, group):
    path = service.root / f"{client}.json"
    unrelated = {"command": "/existing/server", "args": ["unchanged"]}
    path.write_text(json.dumps({group: {"existing": unrelated}, "otherSetting": True}))
    connected = service.cli("connect", client, "--config-path", str(path))
    # Project-scope clients keep the entry `graphicle`; a
    # user-level client's entry is named after the project.
    entry = connected["entry"]
    assert entry == ("graphicle-sources" if client == "cursor" else "graphicle")
    value = json.loads(path.read_text())
    assert value[group]["existing"] == unrelated and value["otherSetting"] is True
    added = value[group][entry]
    assert added["command"] == "graphicle" or Path(added["command"]).samefile(BINARY)
    assert "mcp" in added["args"] and service.session["capability"] not in json.dumps(added)
    assert os.stat(path).st_mode & 0o777 == 0o600
    value[group][entry]["args"] = ["conflicting"]
    path.write_text(json.dumps(value))
    service.cli("connect", client, "--config-path", str(path), ok=False)
    assert json.loads(path.read_text()) == value


def test_hosted_origin_controls_creation_and_oauth_redirect(service):
    service.call("oauth.configure", provider="github", config={
        "client_id": "test-only-client", "client_secret_ref": None,
        "authorization_url": "https://github.com/login/oauth/authorize",
        "token_url": "https://github.com/login/oauth/access_token",
        "identity_url": "https://api.github.com/user", "scopes": ["repo"], "pkce": True})
    service.start("--public-origin", "https://memory.example.test")
    headers = {"Host": "memory.example.test", "Origin": "https://memory.example.test"}
    assert service.http("/health", headers=headers)[1]["hosted"] is True
    assert service.http("/api/workspaces", {"name": "Uninvited"}, headers)[0] == 400
    assert service.http("/health", headers={**headers, "Origin": "http://memory.example.test"})[0] == 403
    code, answer, _ = service.http(f"/api/workspaces/{service.session['workspace']}/oauth/start", {"provider": "github"},
                                  {**headers, "X-Forwarded-Host": "evil.example", "X-Forwarded-Proto": "http"})
    assert code == 200, answer
    assert parse_qs(urlparse(answer["authorization_url"]).query)["redirect_uri"] == ["https://memory.example.test/oauth/callback/github"]


@pytest.mark.skipif(os.name != "posix", reason="POSIX service-manager shutdown")
def test_hosted_service_sigterm_exits_cleanly_and_preserves_store(service):
    service.start("--public-origin", "https://memory.example.test")
    service.process.send_signal(signal.SIGTERM)
    assert service.process.wait(timeout=12) == 0
    service.process = None
    service.start("--public-origin", "https://memory.example.test")
    assert service.http("/health", headers={"Host": "memory.example.test"})[0] == 200
    assert "north entrance" in service.read("file:notes.txt")


def test_provider_permission_mode_requires_the_commercial_edition(service):
    """Core enforces provider permissions on read but does not sync them, so it
    refuses the mode; the commercial edition syncs them, and its own tests
    cover that."""
    if service.call("build.info")["provider_permission_sync"]:
        pytest.skip("this build syncs provider permissions (tested with its edition)")
    arguments = service.root / "provider.json"
    arguments.write_text(json.dumps({"name": "Fixture gdrive", "kind": "gdrive", "credential_env": "GRAPHICLE_FIXTURE_CREDENTIAL",
                                     "config": {"folder_id": "root", "recursive": True, "resolve_groups": True, "permission_mode": "provider",
                                                "acl": {"workspace_readers": True, "credentials": []}}}))
    error = service.cli("call", "connector.add", "--arguments", str(arguments), ok=False)
    assert "is not part of Graphicle Core" in error, error


@pytest.mark.skipif(sys.platform != "darwin", reason="Uses the bundled macOS Vision helper")
def test_real_image_ocr_and_scanned_pdf_are_retained(service):
    script = service.root / "make-scans.swift"
    script.write_text('''import AppKit
import PDFKit
let directory = URL(fileURLWithPath: CommandLine.arguments[1])
let image = NSImage(size: NSSize(width: 1100, height: 240))
image.lockFocus()
NSColor.white.setFill(); NSRect(x: 0,y: 0,width: 1100,height: 240).fill()
("Copperfern launch needs access review." as NSString).draw(at: NSPoint(x: 30,y: 100), withAttributes: [.font: NSFont.systemFont(ofSize: 38), .foregroundColor: NSColor.black])
image.unlockFocus()
let bitmap = NSBitmapImageRep(data: image.tiffRepresentation!)!
try bitmap.representation(using: .png, properties: [:])!.write(to: directory.appendingPathComponent("scanned.png"))
let pdf = PDFDocument(); pdf.insert(PDFPage(image: image)!, at: 0)
pdf.write(to: directory.appendingPathComponent("scanned.pdf"))
''')
    subprocess.run(["swift", str(script), str(service.folder)], check=True, capture_output=True, timeout=60)
    service.cli("sync", str(service.folder))
    for name in ["scanned.png", "scanned.pdf"]:
        assert "Copperfern launch needs access review" in service.read(f"file:{name}")
        original = service.call("document.original", source_id=f"file:{name}")
        assert base64.b64decode(original["base64"]) == (service.folder / name).read_bytes()
        assert original["parser"] == "macOS Vision/PDFKit"
