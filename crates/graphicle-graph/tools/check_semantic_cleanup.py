#!/usr/bin/env python3
"""Compare semantic validation, mutation, cleanup and diagnostics to the pin."""
import contextlib
import copy
import io
import random
import sys
import tempfile
from pathlib import Path
from parity import Native, upstream_tests
from naming import to_upstream
import graphify.semantic_cleanup as reference
import graphify.build as build

native = Native("semantic_cleanup")
def upstream(*args, **kwargs):
    """A native result with engine names mapped back (tools/naming.py)."""
    return to_upstream(native.result(*args, **kwargs))
names = ["validate_semantic_fragment", "load_validated_semantic_fragment", "sanitize_semantic_fragment", "_is_sentence_like_rationale_label", "_append_rationale_attr"]
original = {name: getattr(reference, name) for name in names}
counts = {name: 0 for name in names}
counts["normalize_members"] = 0
def limits():
    return dict(bytes=reference.MAX_SEMANTIC_FRAGMENT_BYTES, nodes=reference.MAX_SEMANTIC_FRAGMENT_NODES,
                edges=reference.MAX_SEMANTIC_FRAGMENT_EDGES, hyperedges=reference.MAX_SEMANTIC_FRAGMENT_HYPEREDGES,
                members=reference.MAX_SEMANTIC_HYPEREDGE_NODES, id=reference.MAX_SEMANTIC_ID_LENGTH)
def captured(function, *args):
    output = io.StringIO()
    with contextlib.redirect_stderr(output): result = function(*args)
    return result, output.getvalue()
def warning_text(messages):
    return "".join(message + "\n" for message in messages)
def warnings(actual, expected, request):
    if warning_text(actual) != expected: native.fail(request=request, expected_warnings=expected, actual_warnings=actual)
    for warning in actual: print(warning, file=sys.stderr)
def validate(fragment):
    counts[names[0]] += 1
    expected = copy.deepcopy(fragment)
    errors, expected_warnings = captured(original[names[0]], expected)
    actual = upstream("semantic.validate", fragment=fragment, limits=limits())
    if actual["errors"] != errors or actual["fragment"] != expected:
        native.fail(operation="validate", input=fragment, limits=limits(), expected={"errors":errors, "fragment":expected}, actual=actual)
    warnings(actual["warnings"], expected_warnings, fragment)
    if isinstance(fragment, dict): fragment.clear(); fragment.update(actual["fragment"])
    return actual["errors"]
def load(path):
    counts[names[1]] += 1
    expected, expected_warnings = captured(original[names[1]], path)
    actual = upstream("semantic.load", path=str(path), limits=limits())
    errors_equal = actual["errors"] == expected[1]
    if expected[1] and expected[1][0].startswith(("invalid JSON:", "could not stat ", "could not read ")):
        errors_equal = len(actual["errors"]) == 1 and actual["errors"][0].split(":",1)[0] == expected[1][0].split(":",1)[0]
    if actual["fragment"] != expected[0] or not errors_equal: native.fail(operation="load", path=str(path), expected=expected, actual=actual)
    warnings(actual["warnings"], expected_warnings, str(path))
    return actual["fragment"], actual["errors"]
def sanitize(fragment):
    counts[names[2]] += 1
    expected = copy.deepcopy(fragment)
    _, expected_warnings = captured(original[names[2]], expected)
    actual = upstream("semantic.sanitize", fragment=fragment)
    if actual["fragment"] != expected: native.fail(operation="sanitize", input=fragment, expected=expected, actual=actual)
    warnings(actual["warnings"], expected_warnings, fragment)
    fragment.clear(); fragment.update(actual["fragment"])
    return fragment
def sentence(label):
    counts[names[3]] += 1
    expected = original[names[3]](label)
    actual = upstream("semantic.sentence", label=label)
    if actual != expected: native.fail(operation="sentence", label=label, expected=expected, actual=actual)
    return actual
def append(node, texts):
    counts[names[4]] += 1
    expected = copy.deepcopy(node)
    original[names[4]](expected, texts)
    actual = upstream("semantic.append", node=node, texts=texts)
    if actual != expected: native.fail(operation="append", node=node, texts=texts, expected=expected, actual=actual)
    node.clear(); node.update(actual)
def members(he):
    counts["normalize_members"] += 1
    expected = copy.deepcopy(he)
    _, expected_warnings = captured(build._normalize_hyperedge_members, expected)
    actual = upstream("semantic.normalize_members", hyperedge=he)
    if actual["hyperedge"] != expected or warning_text(actual["warnings"]) != expected_warnings:
        native.fail(operation="members", input=he, expected=expected, expected_warnings=expected_warnings, actual=actual)
try:
    for name, function in zip(names, [validate,load,sanitize,sentence,append]): setattr(reference,name,function)
    tests = upstream_tests("tests/test_semantic_cleanup.py")
    rng = random.Random(2026092306)
    ids = [None, "", "ok", "日本語", "Σς", "𝟡", "a/b", "a\\b", "..", "a..b", "\n", "a\n", "abc-_.:x", "x"*257, "\u0301", 0, 1, False, [], {}, ["a"]]
    refs = ids + [{"id":x} for x in ids] + [1.5, {"other":"id"}, {"id":1.5}]
    with contextlib.redirect_stderr(io.StringIO()):
        for _ in range(1000):
            he = {"id":rng.choice(ids), rng.choice(["nodes","members","node_ids"]):rng.choices(refs,k=rng.randrange(15))}
            if rng.random()<.25: he["nodes"] = rng.choice([None,"bad",[],["a","b"]])
            members(he)
            validate({"nodes":[{"id":rng.choice(ids),"file_type":rng.choice(["code","rationale","custom"])} for _ in range(rng.randrange(8))],
                      "edges":[{"source":rng.choice(ids),"target":rng.choice(ids)} for _ in range(rng.randrange(5))],"hyperedges":[he]})
        for value in [None,[],0,False,"x",{}, {"nodes":None,"edges":True,"hyperedges":"bad"}, {"nodes":[None,0,False,[]],"edges":[None,3],"hyperedges":[None,[],False]}]:validate(value)
        labels = [None,"",False,0,[],{},"A short name.","This is a reason why a node should be used.","a"*79+".","a"*78+".","word "*7+"word."]
        for _ in range(400): labels.append("".join(rng.choices(["word", "Σ", "é", "\u0301", " ", "\n", "\x1c", "\x85", ".", ":", "!", "?", "\u2003"],k=rng.randrange(100))))
        for label in labels: sentence(label)
        for _ in range(500):
            nodes = [{"id":rng.choice(["", "a","b","c","d","e"]), "label":rng.choice(labels), "file_type":rng.choice(["code","document","rationale","concept","framework"]), "extra":rng.randrange(100)} for _ in range(rng.randrange(14))]
            edges = [{"source":rng.choice(["", "a","b","c","d","e","ghost"]), "target":rng.choice(["", "a","b","c","d","e","ghost"]), "relation":rng.choice(["rationale_for","contains","references"])} for _ in range(rng.randrange(20))]
            hyperedges = [{"id":f"h{i}",rng.choice(["nodes","members","node_ids"]):rng.choices(refs+["a","b","c","d","e"],k=rng.randrange(12))} for i in range(5)]
            sanitize({"nodes":nodes,"edges":edges,"hyperedges":hyperedges,"extra":"retain"})
        for existing in [None,"",False,0,[],{},"Original rationale."]:
            for texts in [[],[""],["New"],[" A "," B "],["\x1cα\x85"]]:append({"id":"x","rationale":existing},texts)
        with tempfile.TemporaryDirectory(prefix="graphify-semantic-") as directory:
            path = Path(directory)/"chunk.json"
            for text in ['{}', '[]', '{"nodes": [{"id":"x"}]}', '{"bad":', '\ufeff{}', '{"a": 1, "a": 2}']:
                path.write_text(text);load(path)
            load(Path(directory)/"missing.json");load(Path(directory))
finally: native.close()
native.receipt("graphify/semantic_cleanup.py",["src/semantic_cleanup.rs","src/hyperedge_members.rs"],checked=counts,upstream_tests=tests,
    comparisons="Ordered validation errors, byte/count limits, in-place normalization, duplicate IDs, rationale edge gating, sentence boundaries, hyperedge aliases/object references, diagnostics and fail-before-parse loading.",
    adaptations=["Rust JSON value API accepts finite JSON numbers and Unicode scalar strings; Python-only objects, arbitrary precision integers and lone surrogates are outside this interface.","Filesystem and JSON parser diagnostics retain error categories/prefixes but use native error descriptions.","Diagnostics are returned explicitly to library callers; CLI/workflow integration must route them to stderr.","Only build.py's shared hyperedge member-normalization dependency is included; build.py remains unported in the ledger."])
