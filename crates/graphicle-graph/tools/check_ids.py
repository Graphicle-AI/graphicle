#!/usr/bin/env python3
"""Differential check of every Unicode scalar and mixed identifiers against upstream."""
import ast
import json
import random
import sys
import unicodedata
from parity import Native, SOURCE
from graphify.ids import normalize_id, make_id

if unicodedata.unidata_version != "15.0.0":
    raise SystemExit("Use the pinned Python 3.12 reference")
native = Native('ids')
count = 0


def compare(values, parts=()):
    global count
    actual = native.call('ids', values=values, parts=parts)
    expected = {"normalized":[normalize_id(value) for value in values], "made":[make_id(*p) for p in parts]}
    if not actual.get("ok") or actual["result"] != expected:
        native.fail(input=values, parts=parts, expected=expected, actual=actual)
    count += len(values) + len(parts)


try:
    batch = []
    for cp in range(0x110000):
        if 0xD800 <= cp <= 0xDFFF:
            continue
        batch.append(chr(cp))
        if len(batch) == 4096:
            compare(batch)
            batch = []
    if batch:
        compare(batch)
    tree = ast.parse((SOURCE / "tests/test_id_normalization_contract.py").read_text())
    constants = {}
    for node in tree.body:
        if isinstance(node, ast.Assign) and isinstance(node.targets[0], ast.Name) and node.targets[0].id in ("CONTRACT_CASES", "CASE_EXPANDING_CHARS"):
            constants[node.targets[0].id] = ast.literal_eval(node.value)
    rng = random.Random(20260923)
    alphabet = list("Iıİi_.-/aZ0é日本語\u0345\u0300\u0301\u01f0\u0f35") + constants["CASE_EXPANDING_CHARS"]
    values = constants["CONTRACT_CASES"] + ["".join(rng.choices(alphabet, k=rng.randrange(17))) for _ in range(10000)]
    parts = [["", "__", "..a_", "_b..", ""], [], ["日本語", "café", "İslemYap"]] + [[rng.choice(values) for _ in range(rng.randrange(6))] for _ in range(1000)]
    for offset in range(0, len(values), 1000):
        compare(values[offset:offset+1000])
    compare([], parts)
finally:
    native.close()

native.receipt('graphify/ids.py', ['src/ids.rs','src/unicode15.rs'], unicode_scalars=1112064,
               checked_values_and_parts=count, unicode_version=unicodedata.unidata_version,
               limitation='Valid Unicode scalar strings. JSON boundaries reject lone UTF-16 surrogate code points.')
