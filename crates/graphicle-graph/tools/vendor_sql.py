#!/usr/bin/env python3
"""Retain the exact generated SQL grammar from the pinned source distribution."""
import hashlib
import io
import json
import tarfile
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
URL = "https://files.pythonhosted.org/packages/e8/5c/3d10387f779f36835486167253682f61d5f4fd8336b7001da1ac7d78f31c/tree_sitter_sql-0.3.11.tar.gz"
SHA = "700b93be2174c3c83d174ec3e10b682f72a4fb451f0076c7ce5012f1d5a76cbc"
data = urllib.request.urlopen(URL, timeout=60).read()
assert hashlib.sha256(data).hexdigest() == SHA
archive = tarfile.open(fileobj=io.BytesIO(data))
files = {}
for name in ["LICENSE", "src/parser.c", "src/scanner.c", "src/tree_sitter/parser.h", "src/tree_sitter/alloc.h", "src/tree_sitter/array.h"]:
    entry = archive.getmember("tree_sitter_sql-0.3.11/" + name)
    assert entry.isfile()
    blob = archive.extractfile(entry).read()
    relative = name.replace("src/", "grammar/", 1)
    destination = ROOT / "vendor/tree-sitter-sql" / relative
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(blob)
    files[relative] = hashlib.sha256(blob).hexdigest()
receipt = {"url": URL, "sha256": SHA, "files": files,
           "note": "Native parser dependency. Python's pinned 0.3.11 uses tree-sitter ABI 15; crates.io sequel 0.3.11 ships ABI 14. Retain the exact pinned generated source."}
(ROOT / "vendor/tree-sitter-sql/SOURCE.json").write_text(json.dumps(receipt, indent=2) + "\n")
print(json.dumps(receipt))
