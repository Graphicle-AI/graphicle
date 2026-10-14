#!/usr/bin/env python3
"""Retain each non-Python reference asset, with per-file source/destination hashes.

Original documentation and configs are references, not claims that the unfinished
native application implements everything they describe.
"""
import argparse
import datetime
import hashlib
import json
import shutil
from pathlib import Path

# --refresh re-pins the retained copies after the ledger moves to a new
# upstream commit: stale copies are replaced and assets the previous receipt
# retained but the new ledger no longer lists are removed. Without it, any
# difference from the pinned ledger is an error.
arguments = argparse.ArgumentParser()
arguments.add_argument("--refresh", action="store_true")
refresh = arguments.parse_args().refresh
root = Path(__file__).resolve().parents[1]
path = root / "migration/FILES.json"
ledger = json.loads(path.read_text())
source = Path(ledger["source_root"])
categories = {"documentation_or_prompt_asset", "reference_fixture", "license_notice", "other_asset_or_configuration"}
receipt = {"upstream_commit":ledger["upstream_commit"], "checked_utc":datetime.datetime.now(datetime.timezone.utc).isoformat(), "files":[]}
previous_receipt = root / "migration/receipts/retained-assets.json"
if refresh and previous_receipt.exists():
    listed = {row["path"] for row in ledger["files"] if row["category"] in categories}
    for old in json.loads(previous_receipt.read_text())["files"]:
        stale = root / old["target"]
        if old["source"] not in listed and stale.is_file():
            assert stale.is_relative_to(root / "upstream-assets")
            stale.unlink()
for row in ledger["files"]:
    if row["category"] not in categories:
        continue
    original = source / row["path"]
    destination = root / "upstream-assets" / row["path"]
    assert original.is_relative_to(source) and destination.is_relative_to(root / "upstream-assets")
    assert hashlib.sha256(original.read_bytes()).hexdigest() == row["sha256"]
    if destination.exists() and refresh and hashlib.sha256(destination.read_bytes()).hexdigest() != row["sha256"]:
        shutil.copy2(original, destination)
    if destination.exists():
        assert hashlib.sha256(destination.read_bytes()).hexdigest() == row["sha256"], str(destination)
    else:
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(original, destination)
    assert hashlib.sha256(destination.read_bytes()).hexdigest() == row["sha256"]
    target = str(destination.relative_to(root))
    row.update(status="retained_reference_verified", retained_target=target,
               evidence=["migration/receipts/retained-assets.json"])
    receipt["files"].append({"source":row["path"],"target":target,"sha256":row["sha256"]})
(root / "migration/receipts/retained-assets.json").write_text(json.dumps(receipt,indent=2) + "\n")
path.write_text(json.dumps(ledger,indent=2,ensure_ascii=False) + "\n")
print(json.dumps({"retained_files":len(receipt["files"]),"application_completion_unchanged":True}))
