#!/usr/bin/env python3
"""Account for every file in the pinned archive; never count local caches as source."""
import ast
import collections
import datetime
import hashlib
import json
import os
import tarfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT.parents[1]
# The folder holding the pinned GitHub archive and its extracted tree;
# GRAPHICLE_UPSTREAM_DOWNLOAD names another copy (a checkout without the
# research folder).
DOWNLOAD = Path(os.environ.get("GRAPHICLE_UPSTREAM_DOWNLOAD")
                or PROJECT / "research/memory-runtime-integration-2026-09-22/upstream")
SOURCE = DOWNLOAD / "Graphify-Labs__graphify-0.9.88"
ARCHIVE = DOWNLOAD / "Graphify-Labs__graphify-a9860f70dc3c.tar.gz"
COMMIT = "a9860f70dc3cc2e9c2d8cb923484cfa0592b9067"
VERSION = "0.9.88"


def category(path):
    parts = path.parts
    if parts[:2] == ("tests", "fixtures") or parts[0] == "worked":
        return "reference_fixture"
    if path.suffix == ".py":
        if parts[0] == "graphify":
            return "application_python"
        if parts[0] == "tests":
            return "upstream_test_python"
        return "development_tool_python"
    if path.name in ("LICENSE", "LICENSE-MIT", "NOTICE"):
        return "license_notice"
    if path.suffix == ".md":
        return "documentation_or_prompt_asset"
    if parts[0] == ".github" or path.name in ("pyproject.toml", "uv.lock", "MANIFEST.in"):
        return "build_or_ci_configuration"
    return "other_asset_or_configuration"


def target(path, kind):
    if kind == "application_python":
        parts = list(path.parts[1:])
        if parts == ["__init__.py"]:
            return "src/lib.rs"
        if parts == ["__main__.py"]:
            return "src/entrypoint.rs"
        parts[-1] = "mod.rs" if parts[-1] == "__init__.py" else path.stem + ".rs"
        return str(Path("src", *parts))
    if kind == "upstream_test_python":
        return None
    if kind == "development_tool_python":
        return str(Path("src/development_tools", *path.parts).with_suffix(".rs"))
    return None


def main():
    destination = ROOT / "migration"
    destination.mkdir(parents=True, exist_ok=True)
    previous = {}
    if (destination / "FILES.json").exists():
        ledger = json.loads((destination / "FILES.json").read_text())
        # A new pin starts a fresh ledger: every application file must earn a
        # receipt against the new pinned source before it counts as ported.
        if ledger["upstream_commit"] == COMMIT:
            previous = {row["path"]: row for row in ledger["files"]}
    rows = []
    with tarfile.open(ARCHIVE) as archive:
        members = [member for member in archive.getmembers() if member.isfile() or member.issym()]
        prefix = members[0].name.split("/")[0] + "/"
        for member in sorted(members, key=lambda member: member.name):
            relative = member.name.removeprefix(prefix)
            path = Path(relative)
            if path.is_absolute() or ".." in path.parts:
                raise ValueError("Archive contains an unsafe path")
            if member.issym():
                raise ValueError("A new symlink needs an explicit preservation policy")
            data = archive.extractfile(member).read()
            digest = hashlib.sha256(data).hexdigest()
            if hashlib.sha256((SOURCE / path).read_bytes()).hexdigest() != digest:
                raise ValueError("Extracted upstream file differs from pinned archive: " + relative)
            kind = category(path)
            row = {"path": relative, "sha256": digest, "bytes": len(data), "lines": len(data.splitlines()), "category": kind, "rust_target": target(path, kind), "status": "pending", "evidence": []}
            if path.suffix == ".py":
                tree = ast.parse(data, filename=relative)
                row["definitions"] = [{"name":node.name,"line":node.lineno,"kind":type(node).__name__} for node in ast.walk(tree) if isinstance(node,(ast.FunctionDef,ast.AsyncFunctionDef,ast.ClassDef))]
                row["imports"] = sorted({node.module or "." for node in ast.walk(tree) if isinstance(node,ast.ImportFrom)} | {name.name for node in ast.walk(tree) if isinstance(node,ast.Import) for name in node.names})
            else:
                row["disposition"] = "Retain in its source format or explicitly migrate configuration; do not silently omit."
            if kind == "reference_fixture":
                row["disposition"] = "Preserve original bytes and language: changing the fixture to Rust would change the parser input."
            if old := previous.get(relative):
                if old["sha256"] != digest:
                    raise ValueError("Pinned source changed; freeze a separate migration target")
                for key in ("status", "evidence", "rust_target", "rust_files", "notes", "checked_utc"):
                    if key in old:
                        row[key] = old[key]
            if kind == "upstream_test_python":
                row.update(status="retained_python_reference", rust_target=None,
                           notes="User clarified on 2026-09-23: retain original tests as behavioral checks; do not rewrite the test suite into Rust.")
            rows.append(row)
    names = {row["path"] for row in rows}
    extras = sorted(str(path.relative_to(SOURCE)) for path in SOURCE.rglob("*") if (path.is_file() or path.is_symlink()) and str(path.relative_to(SOURCE)) not in names)
    counts = dict(collections.Counter(row["category"] for row in rows))
    result = {"upstream_commit": COMMIT, "upstream_version": VERSION, "source_root": str(SOURCE), "archive": str(ARCHIVE), "archive_sha256": hashlib.sha256(ARCHIVE.read_bytes()).hexdigest(), "archive_files": len(rows), "application_python_lines": sum(row["lines"] for row in rows if row["category"] == "application_python"), "category_counts": counts, "local_extra_files": extras, "all_archive_files_verified": True, "generated_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(), "files": rows}
    (destination / "FILES.json").write_text(json.dumps(result, indent=2, ensure_ascii=False) + "\n")
    print(json.dumps({key: result[key] for key in ("upstream_commit","archive_files","application_python_lines","category_counts","all_archive_files_verified")}))


if __name__ == "__main__":
    main()
