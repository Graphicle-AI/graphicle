#!/usr/bin/env python3
"""Copy only the pinned detect constants needed by the Markdown port."""
import ast
import json
from pathlib import Path

root = Path(__file__).resolve().parents[1]
ledger = json.loads((root / "migration/FILES.json").read_text())
tree = ast.parse((Path(ledger["source_root"]) / "graphify/detect.py").read_text())
names = {"CODE_EXTENSIONS", "DOC_EXTENSIONS", "_SKIP_DIRS"}
values = {}
for node in tree.body:
    if isinstance(node, ast.Assign):
        for target in node.targets:
            if isinstance(target, ast.Name) and target.id in names:
                values[target.id] = sorted(ast.literal_eval(node.value))
assert set(values) == names
text = "// Constant data from pinned graphify/detect.py; this does not port its scanner.\n"
for name, items in values.items():
    text += f"pub const {name.lstrip('_')}: &[&str] = &[\n"
    text += "".join("    " + json.dumps(item) + ",\n" for item in items) + "];\n"
(root / "src/extractors/markdown_data.rs").write_text(text)
print(json.dumps({name: len(items) for name, items in values.items()}))
