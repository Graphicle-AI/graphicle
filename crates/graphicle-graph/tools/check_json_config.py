#!/usr/bin/env python3
"""Compare bounded JSON AST extraction against the identical grammar release."""
import ast
import json
import random
import tempfile
from pathlib import Path
from parity import Native, SOURCE, upstream_tests
import graphify.extractors.json_config as reference

native = Native("json_config")
original = reference.extract_json
checked = 0


def extract(path):
    global checked
    checked += 1
    actual = native.result("extract.json", path=str(path))
    expected = original(path)
    if actual != expected:
        native.fail(path=str(path), source=path.read_text(errors="replace")[:20000] if path.is_file() else None,
                    expected=expected, actual=actual)
    return actual


try:
    reference.extract_json = extract
    tree = ast.parse((SOURCE / "tests/test_extract.py").read_text())
    names = [node.name for node in tree.body if isinstance(node,ast.FunctionDef) and node.name.startswith("test_extract_json_")]
    tests = upstream_tests(*["tests/test_extract.py::"+name for name in names])
    rng = random.Random(20260923)
    with tempfile.TemporaryDirectory(prefix="graphify-json-parity-") as directory:
        root = Path(directory)
        extract(root / "missing.json")
        extract(root)
        for filename in ["package.json","arbitrary.json","API.tsconfig.json","PACKAGE.JSON","manifeſt.json"]:
            path = root / filename
            cases = [b"", b"[]", b"null", b"{}", b'{"dependencies":{"react":"1", "react":"2"}}',
                     b'{"extends":["a", "b", 3], "exclude":["x"]}', b'{"//":"comment", "___": 1}',
                     b'{"dependencies":{"broken":"a\\nb", "\xff": "1"}}',
                     b'// before\n{"dependencies":{}}', b'{"compilerOptions": { /*comment*/ "strict":true,}}',
                     b'{"$ref":"#/definitions/a"}', b'{"extends": "a", "extends": "b"}',
                     json.dumps({"dependencies":{f"pkg{i}":"1" for i in range(510)}}).encode()]
            nested = {"dependencies":{"package":"1"}}
            for _ in range(10): nested = {"compilerOptions": nested}
            cases.append(json.dumps(nested).encode())
            for data in cases:
                path.write_bytes(data)
                extract(path)
            for _ in range(60):
                keys = rng.sample(["dependencies","devDependencies","extends","$ref","compilerOptions","name","", "a/b", "日本語", "___", "//"],rng.randrange(1,10))
                obj = {key:rng.choice(["1", None, True, "日本語", {"a":"x","b":"y"}, ["base",2,"strict"]]) for key in keys}
                path.write_text(json.dumps(obj,ensure_ascii=rng.choice([True,False]),indent=rng.choice([None,2])))
                extract(path)
        path = root / "package.json"
        for count in [1_048_575,1_048_576,1_048_577]:
            path.write_bytes(b" " * count)
            extract(path)
finally:
    native.close()

native.receipt("graphify/extractors/json_config.py", ["src/extractors/json_config.rs"],
               extractions_checked=checked, upstream_tests=tests, grammar_version="tree-sitter-json 0.24.8",
               comparisons="Complete ordered extraction JSON, duplicate keys, malformed input, exact byte/pair/depth caps and all config recognition paths.",
               test_scope="JSON extraction is native. Orchestration/build steps in integration fixtures still use unported upstream Python.")
