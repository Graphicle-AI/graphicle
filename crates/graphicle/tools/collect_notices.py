#!/usr/bin/env python3
"""Collect actual dependency license files and pinned upstream source receipts.

Writes the receipt DEPENDENCIES.json and the per-crate texts under licenses/rust
(local, not tracked). The shipped licence page is THIRD_PARTY_LICENSES.html,
which `cargo about` generates from Cargo.lock with about.toml (CONTRIBUTING.md).
"""
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parents[1]
CORE = ROOT.parent / "graphicle-memory"
PROJECT = ROOT.parents[1]


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def collect():
    metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--locked", "--format-version", "1"], cwd=ROOT))
    lock = tomllib.loads((ROOT / "Cargo.lock").read_text())
    checksums = {(p["name"], p["version"]): p.get("checksum") for p in lock["package"]}
    destination = ROOT / "licenses"
    destination.mkdir(exist_ok=True)
    generated = destination / "rust"
    if generated.exists():
        shutil.rmtree(generated)  # regenerated output only; upstream inputs live separately
    # Preserve notices for adapted upstream code separately from linked crates.
    for source in (CORE / "licenses").iterdir():
        if source.is_file():
            shutil.copy2(source, destination / source.name)
    dependencies = []
    missing = []
    for package in sorted(metadata["packages"], key=lambda p: (p["name"], p["version"])):
        directory = Path(package["manifest_path"]).parent
        files = set()
        # Path crates carry their LICENSE beside the manifest: Apache-2.0 for
        # the first-party crates, the upstream text for the vendored ones (or
        # Groovy's upstream licence declaration). Their licenses/ directories
        # are third-party output, never input to themselves.
        for path in directory.iterdir() if package.get("source") else []:
            if path.is_file() and re.match(r"^(licen[sc]e|copying|notice|copyright)([._-]|$)", path.name, re.I):
                files.add(path)
            if path.is_dir() and path.name.lower() in {"licenses", "licences"}:
                files.update(p for p in path.rglob("*") if p.is_file())
        if not package.get("source"):
            files.update(directory / name for name in ("LICENSE", "UPSTREAM-LICENSE-DECLARATION.toml")
                         if (directory / name).is_file())
        # r-efi includes its complete MIT permission and copyright notice in
        # AUTHORS instead of a file named LICENSE (the crate is triple-licensed).
        if package["name"] == "r-efi":
            files.add(directory / "AUTHORS")
        if package.get("license_file"):
            license_file=Path(package["license_file"])
            files.add(license_file if license_file.is_absolute() else (directory / license_file).resolve())
        for fallback in (CORE / "licenses/rust" / f"{package['name']}-{package['version']}", destination / "upstream-crates" / f"{package['name']}-{package['version']}"):
            if fallback.is_dir():
                files.update(p for p in fallback.rglob("*") if p.is_file())
        texts = []
        for path in sorted(files):
            if path.stat().st_size > 1024 * 1024:
                continue
            try:
                value = path.read_text()
            except UnicodeError:
                continue
            if not value.strip():
                continue
            relative = str(path.relative_to(directory)) if path.is_relative_to(directory) else path.name
            name = f"{package['name']}-{package['version']}__{relative.replace('/', '__')}"
            saved = destination / "rust" / name
            saved.parent.mkdir(exist_ok=True)
            saved.write_text(value)
            texts.append({"file": str(saved.relative_to(ROOT)), "sha256": sha(saved)})
        if not texts:
            missing.append({"name": package["name"], "version": package["version"], "license": package.get("license"), "manifest": str(directory)})
        dependencies.append({"name": package["name"], "version": package["version"], "declared_license": package.get("license"),
                             "repository": package.get("repository"), "source": package.get("source"),
                             "crate_checksum": checksums.get((package["name"], package["version"])), "license_texts": texts,
                             **({"selected_license": "MIT"} if package["name"] == "r-efi" else {})})
    graph = {"format": "graphicle.dependency-notices.v1", "scope": "Cargo locked dependency graph, including build/dev/platform packages; not all packages execute in every build",
             "cargo_lock_sha256": sha(ROOT / "Cargo.lock"), "packages": dependencies, "missing_package_license_texts": missing}
    (ROOT / "DEPENDENCIES.json").write_text(json.dumps(graph, indent=2) + "\n")
    print(f"Collected license files for {len(dependencies) - len(missing)}/{len(dependencies)} packages.", flush=True)
    if missing:
        print(json.dumps(missing, indent=2), flush=True)
        raise SystemExit("Resolve missing package notices before distributing.")


if __name__ == "__main__":
    collect()
