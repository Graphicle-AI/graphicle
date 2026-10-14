#!/usr/bin/env python3
"""Compare both form parsers, including hierarchy, event provenance and failures."""
import ast
import random
import tempfile
from pathlib import Path
from parity import Native, SOURCE, upstream_tests
import graphify.extractors.pascal_forms as reference

native = Native("pascal_forms")
originals = {"lazarus_form": reference.extract_lazarus_form, "delphi_form":reference.extract_delphi_form}
checked = 0


def extract(kind,path):
    global checked
    checked += 1
    actual = native.result("extract."+kind, path=str(path))
    expected = originals[kind](path)
    if actual != expected:
        native.fail(kind=kind, path=str(path), source=path.read_text(errors="replace") if path.is_file() else None,
                    expected=expected, actual=actual)
    return actual


try:
    reference.extract_lazarus_form = lambda path: extract("lazarus_form",path)
    reference.extract_delphi_form = lambda path: extract("delphi_form",path)
    tree = ast.parse((SOURCE / "tests/test_pascal.py").read_text())
    names = [node.name for node in tree.body if isinstance(node,ast.FunctionDef) and node.name.startswith(("test_lfm_","test_dfm_")) and not node.name.endswith(("dispatch_registered","extension_registered"))]
    tests = upstream_tests(*["tests/test_pascal.py::"+name for name in names])
    rng = random.Random(20260923)
    with tempfile.TemporaryDirectory(prefix="graphify-form-parity-") as directory:
        root = Path(directory)
        for kind in originals:
            extract(kind,root / "missing.form")
            extract(kind,root)
            path = root / ("ui.lfm" if kind == "lazarus_form" else "ui.dfm")
            for raw in [b"\xff\x0a\x00", b"TPF0\x00", b"", b"\xff"]:
                path.write_bytes(raw)
                extract(kind,path)
            for case in range(150):
                lines = ["object Form1: TForm1"]
                for _ in range(rng.randrange(1,40)):
                    name = rng.choice(["TButton", "TPanel", "日本語", "é", "a\u0301", "x", "_", "Į"])
                    lines.append(rng.choice([f"object child: {name}", f"OnClick = {name}", "end", f"OBJECT child: {name}", "  END", "On = X", "other = X", "\x1cobject child: TTest"]))
                separator = rng.choice(["\n","\r\n","\r","\u2028","\x0b"])
                path.write_bytes(separator.join(lines).encode())
                extract(kind,path)
finally:
    native.close()

native.receipt("graphify/extractors/pascal_forms.py", ["src/extractors/pascal_forms.rs", "src/io_compat.rs"],
               extractions_checked=checked, upstream_tests=tests,
               comparisons="Both entire APIs: ordered node/edge JSON, duplicate classes/handlers, source line numbers, binary forms and OS errors.")
