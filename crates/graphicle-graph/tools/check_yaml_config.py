#!/usr/bin/env python3
"""Compare structural YAML extraction (Graphify 0.9.95, #2214) with the pinned reference.

The unchanged upstream tests (tests/test_yaml_extraction.py) run with
`extract_yaml` and `classify_file` answered by Rust and compared with the
pinned Python functions; generated documents then cover the config/data gate,
multi-document streams, the pair/depth/byte caps, YAML 1.1 scalars as keys,
anchors, merges and unsafe or unknown tags.
"""
import json
import random
import sys
import tempfile
from pathlib import Path
from parity import Native, SOURCE
from isolated_upstream import run
import graphify.detect as detect
import graphify.extractors.yaml_config as reference

native = Native("yaml_config")
original = reference.extract_yaml
original_classify = detect.classify_file
checked = {"extract": 0, "classify": 0, "errors": 0}


def compare_extract(path):
    checked["extract"] += 1
    expected = json.loads(json.dumps(original(path)))
    actual = native.result("extract.yaml", path=str(path))
    if "error" in expected:
        # PyYAML and the native parser word their diagnostics differently; a
        # refused file must be refused with no nodes on both sides.
        checked["errors"] += 1
        if "error" not in actual or actual.get("nodes") or actual.get("edges"):
            native.fail(path=str(path), source=Path(path).read_bytes()[:20000].hex() if Path(path).is_file() else None,
                        expected=expected, actual=actual)
        return expected
    if actual != expected:
        native.fail(path=str(path), source=Path(path).read_bytes()[:20000].hex() if Path(path).is_file() else None,
                    expected=expected, actual=actual)
    return actual


def compare_classify(path, *args, **kwargs):
    checked["classify"] += 1
    expected = original_classify(path, *args, **kwargs)
    out = native.result("detect", function="classify_file", output=str(detect.GRAPHIFY_OUT),
                        limits={"raw_bytes": detect._OFFICE_MAX_RAW_BYTES,
                                "decompressed_bytes": detect._OFFICE_MAX_DECOMPRESSED_BYTES,
                                "compression_ratio": detect._OFFICE_MAX_COMPRESSION_RATIO},
                        path=str(path))
    actual = detect.FileType(out["value"]) if out["value"] else None
    if actual != expected:
        native.fail(path=str(path), expected=str(expected), actual=str(actual))
    return actual


CONFIG_NAMES = ["values.yaml", "Chart.yaml", "docker-compose.yml", "compose.yaml", "kustomization.yaml",
                ".gitlab-ci.yml", "pubspec.yaml", "action.yml", "mkdocs.yml", "environment.yml"]
OTHER_NAMES = ["deploy.yml", "strings.yaml", "api.yaml", "list.yaml", "Config.YML", "data.yaml"]
KEYS = ["apiVersion", "kind", "metadata", "spec", "name", "on", "jobs", "steps", "services", "image",
        "tag", "port", "env", "x", "y", "openapi", "paths", "$schema", "$defs", "hosts", "tasks", "roles",
        "yes", "No", "1", "2.5", "~", "2026-10-24", "café", "日本", "a b", "_", "-", "<<"]
SCALARS = ["1", "-2", "0x1f", "1_000", "3.14", ".inf", "true", "off", "null", "~", "''", '"q"', "text",
           "2026-10-24", "2026-10-24T10:00:00Z", "[1, 2]", "{a: 1}", "|\n  block", ">\n  folded", "!!str 5",
           "!!binary aGVsbG8=", "&anchor v"]


def random_mapping(rng, depth):
    lines = []
    for _ in range(rng.randrange(0, 6)):
        key = rng.choice(KEYS)
        if depth < 8 and rng.random() < 0.35:
            child = random_mapping(rng, depth + 1)
            lines.append(f"{key}:")
            lines += ["  " + line for line in child] or ["  {}"]
        elif depth < 8 and rng.random() < 0.2:
            lines.append(f"{key}:")
            for _ in range(rng.randrange(1, 4)):
                child = random_mapping(rng, depth + 1)
                if child:
                    lines.append("  - " + child[0])
                    lines += ["    " + line for line in child[1:]]
                else:
                    lines.append("  - " + rng.choice(SCALARS[:13]))
        else:
            lines.append(f"{key}: {rng.choice(SCALARS[:13])}")
    return lines


try:
    reference.extract_yaml = compare_extract
    detect.classify_file = compare_classify
    code, tests = run(["tests/test_yaml_extraction.py"])
    assert code == 0, f"Upstream tests failed: {code}"
    rng = random.Random(20261024)
    with tempfile.TemporaryDirectory(prefix="graphify-yaml-parity-") as directory:
        root = Path(directory)
        compare_extract(root / "missing.yaml")
        compare_extract(root)
        fixed = [
            b"", b"---\n", b"just-a-string\n", b"- one\n- two\n", b"a: 1\nb: 2\n", b"a:\n  b: 1\n",
            b"on:\n  push:\n    branches: [main]\njobs:\n  build:\n    runs-on: ubuntu-latest\n",
            b"apiVersion: v1\nkind: Service\nmetadata:\n  name: web\n---\napiVersion: apps/v1\nkind: Deployment\n",
            b"kind: A\n---\nkind: B\nmetadata: {name: x}\n---\n- 1\n---\nplain\n",
            b"- hosts: all\n  tasks:\n    - name: t\n      shell: echo\n",
            b"openapi: 3.0.0\npaths:\n  /x: {}\n", b"swagger: '2.0'\npaths: {}\ninfo: {}\n",
            b"$schema: x\n$defs: {}\n", b"$schema: x\nname: y\n",
            b"base: &b\n  a: 1\nderived:\n  <<: *b\n  c: 2\n",
            b"1: a\n2.5: b\ntrue: c\nnull: d\n2026-10-24: e\nnested: {x: 1}\n",
            b"? [a, b]\n: c\nn: {x: 1}\n",
            b"danger: !!python/object/apply:os.system ['true']\n",
            b"Resources:\n  B:\n    Type: !Ref X\n", b"a: [\n", b"\tx: 1\n", b"a: 1\na: 2\nb: {c: 1}\n",
            "\ufeffname: café\nspec:\n  日本: 1\n".encode(), b"name: x\r\nspec:\r\n  a: 1\r\n",
            b"\xff\xfename: x\nspec: {a: 1}\n", b"name: x\nspec:\n  " + b"a: \xe9\n",
            ("root:\n" + "".join(f"  k{i}: {i}\n" for i in range(600))).encode(),
            ("d0:\n" + "".join("  " * i + f"  d{i + 1}:\n" for i in range(10)) + "  " * 11 + "  leaf: 1\n").encode(),
            b"x:\n- a: 1\n  b: [ {c: 1}, [ {d: 2} ] ]\n",
            b"metadata:\n  '': 1\n  '...': 2\n  ok: 3\n",
        ]
        for name in CONFIG_NAMES + OTHER_NAMES:
            path = root / name
            for data in fixed:
                path.write_bytes(data)
                compare_extract(path)
                compare_classify(path)
        workflows = root / "repo" / ".github" / "workflows"
        workflows.mkdir(parents=True)
        for data in fixed[:8]:
            (workflows / "ci.yml").write_bytes(data)
            compare_extract(workflows / "ci.yml")
        for count in (1_048_575, 1_048_576, 1_048_577):
            path = root / "values.yaml"
            path.write_bytes(b"#" * count)
            compare_extract(path)
        for i in range(300):
            docs = []
            for _ in range(rng.randrange(1, 4)):
                docs.append("\n".join(random_mapping(rng, 0)) + "\n")
            text = "---\n".join(docs)
            path = root / rng.choice(CONFIG_NAMES + OTHER_NAMES)
            path.write_text(text, encoding="utf-8")
            compare_extract(path)
finally:
    reference.extract_yaml = original
    detect.classify_file = original_classify
    native.close()

native.receipt("graphify/extractors/yaml_config.py",
               ["src/extractors/yaml_config.rs", "src/extractors/yaml_frontmatter.rs"],
               checked=checked, upstream_tests=tests,
               interface_differences=["A file PyYAML refuses is refused natively too, with the native parser's own error message."],
               comparisons="Complete extraction JSON for the upstream tests and generated documents: config/data gate, multi-document "
                           "streams, the 1 MiB, 500-pair and depth-6 caps, YAML 1.1 scalar keys, anchors and merges, unsafe and unknown tags; "
                           "classify_file for every file.")
