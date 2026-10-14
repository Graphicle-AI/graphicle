#!/usr/bin/env python3
"""Refresh the shared core's notices from the verified service dependency set."""
import hashlib
import json
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]
CORE = ROOT.parent / "graphicle-memory"


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--offline", "--format-version", "1"], cwd=CORE))
    service = json.loads((ROOT / "DEPENDENCIES.json").read_text())
    indexed = {(p["name"], p["version"]): p for p in service["packages"]}
    packages = []
    for package in sorted(metadata["packages"], key=lambda p: (p["name"], p["version"])):
        item = indexed.get((package["name"], package["version"]))
        if item is None or not item["license_texts"]:
            raise SystemExit(f"Collect the service notices first: {package['name']} {package['version']}")
        for notice in item["license_texts"]:
            source = ROOT / notice["file"]
            if sha(source) != notice["sha256"]:
                raise SystemExit(f"Changed license: {source}")
            target = CORE / notice["file"]
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, target)
        packages.append(item)
    grammar = json.loads((ROOT / "GRAMMAR_NOTICES.json").read_text())
    for project in grammar["projects"]:
        source = ROOT / project["license_file"]
        if sha(source) != project["license_sha256"]:
            raise SystemExit(f"Changed grammar license: {source}")
        target = CORE / project["license_file"]
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, target)
    shutil.copy2(ROOT / "GRAMMAR_NOTICES.json", CORE / "GRAMMAR_NOTICES.json")
    for name in ("CRATE_SOURCE_NOTICES.json", "DECLARED_CRATE_NOTICES.json"):
        shutil.copy2(ROOT / "licenses" / name, CORE / "licenses" / name)
    record = json.loads((CORE / "UPSTREAM.json").read_text())
    record["projects"][0]["adaptation"] = (
        "49 pinned syntax grammar selections, declarations, imports, calls and original source coordinates; "
        "expanded generic language analyses remain incomplete and do not claim compiler type resolution.")
    record["rust_dependencies"] = packages
    record["cargo_lock_sha256"] = sha(CORE / "Cargo.lock")
    record["grammar_notices"] = "GRAMMAR_NOTICES.json"
    record["dependency_scope"] = "Locked core graph, including build/platform crates; not every package executes on every platform."
    (CORE / "UPSTREAM.json").write_text(json.dumps(record, indent=2) + "\n")
    notice = CORE / "NOTICE.md"
    notice.write_text(notice.read_text().replace(
        "Tree-sitter extraction, source coordinates and ambiguity-aware structural relationships; four language families only.",
        record["projects"][0]["adaptation"]).replace(
        "their versions and declared license expressions are listed in UPSTREAM.json.",
        "their versions and declared license expressions are listed in UPSTREAM.json. Grammar-specific licenses and pinned revisions are in GRAMMAR_NOTICES.json. Declared-license fallbacks are identified in licenses/DECLARED_CRATE_NOTICES.json; they are not represented as recovered upstream copyright notices."))
    print(f"Refreshed {len(packages)} core dependency records and {len(grammar['projects'])} grammar notices.")


if __name__ == "__main__":
    main()
