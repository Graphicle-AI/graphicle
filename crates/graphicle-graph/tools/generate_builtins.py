#!/usr/bin/env python3
"""Copy the pinned extractor's constant set into native Rust data."""
import ast
import json
from pathlib import Path
ROOT = Path(__file__).resolve().parents[1]
ledger = json.loads((ROOT / "migration/FILES.json").read_text())
source = Path(ledger["source_root"]) / "graphify/extractors/base.py"
tree = ast.parse(source.read_text())
node = next(node for node in tree.body if isinstance(node, ast.AnnAssign) and node.target.id == "_LANGUAGE_BUILTIN_GLOBALS")
values = sorted(ast.literal_eval(node.value.args[0]))
text = "// Generated from the pinned graphify/extractors/base.py constant; Apache-2.0.\n"
text += "pub const LANGUAGE_BUILTIN_GLOBALS: &[&str] = &[\n"
text += "".join("    " + json.dumps(value) + ",\n" for value in values) + "];\n"
(ROOT / "src/extractors/builtins_data.rs").write_text(text)
print(json.dumps({"builtin_names":len(values)}))
