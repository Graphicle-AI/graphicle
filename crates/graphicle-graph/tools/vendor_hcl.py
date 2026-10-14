#!/usr/bin/env python3
"""Pin the generated native HCL grammar, independently of its Rust release."""
import hashlib
import io
import json
import tarfile
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
URL = "https://files.pythonhosted.org/packages/06/c9/ed79f643b0cec3e123171c09caffb6088a6111025a20fc69112b1468828b/tree_sitter_hcl-1.2.0.tar.gz"
SHA = "f86cb7a9fd5cb93d83e2f788ae155544464c47755d09190505de562c0d6ad1dd"
data = urllib.request.urlopen(URL, timeout=60).read()
assert hashlib.sha256(data).hexdigest() == SHA
archive = tarfile.open(fileobj=io.BytesIO(data))
files = {}
for name in ["LICENSE", "src/parser.c", "src/scanner.c", "src/tree_sitter/parser.h", "src/tree_sitter/alloc.h", "src/tree_sitter/array.h"]:
    entry = archive.getmember("tree_sitter_hcl-1.2.0/" + name)
    assert entry.isfile()
    blob = archive.extractfile(entry).read()
    relative = name.replace("src/", "grammar/", 1)
    destination = ROOT / "vendor/tree-sitter-hcl" / relative
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(blob)
    files[relative] = hashlib.sha256(blob).hexdigest()
receipt = {"url": URL, "sha256": SHA, "files": files, "note": "Native dependency, not Graphify application code. Exact generated parser differs from crates.io 1.1.0."}
(ROOT / "vendor/tree-sitter-hcl/SOURCE.json").write_text(json.dumps(receipt, indent=2) + "\n")
print(json.dumps(receipt, indent=2))
