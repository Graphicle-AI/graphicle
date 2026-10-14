#!/usr/bin/env python3
"""Compare all validator entry points and run its unchanged upstream tests."""
import copy
import random
from parity import Native, upstream_tests
import graphify.validate as reference

native = Native("validate")
original_validate = reference.validate_extraction


def validate(data):
    actual = native.call("validate", data=data)
    try:
        expected = original_validate(data)
    except TypeError as error:
        if actual.get("ok") or actual.get("error") != "TypeError: " + str(error):
            native.fail(data=data, expected="TypeError: " + str(error), actual=actual)
        raise
    if not actual.get("ok") or sorted(actual["result"]["errors"]) != sorted(expected):
        native.fail(data=data, expected=expected, actual=actual)
    assert actual["result"]["valid"] == (expected == [])
    return actual["result"]["errors"]


def assert_valid(data):
    # Verify the native assertion independently of the native validation API.
    actual = native.call("assert_valid", data=data)
    try:
        expected = original_validate(data)
    except TypeError as error:
        if actual.get("ok") or actual.get("error") != "TypeError: " + str(error):
            native.fail(data=data, expected="TypeError: " + str(error), actual=actual)
        raise
    if not expected:
        assert actual == {"ok": True, "result": None}, actual
        return
    if actual.get("ok"):
        native.fail(data=data, expected=expected, actual=actual)
    heading, *errors = actual["error"].split("\n  • ")
    if heading != f"Extraction JSON has {len(expected)} error(s):" or sorted(errors) != sorted(expected):
        native.fail(data=data, expected=expected, actual=actual)
    raise ValueError(actual["error"])


rng = random.Random(20260923)
values = [None, True, False, 0, 1, -1, 1.0, -0.0, 0.1, 1e-4, 1e-5, 1e-6,
          1e15, 1e16, 1e17, 2**53, 2**53 + 1, 2**64 - 1, [], ["x", None], {},
          {"b": 2, "a": "x"}, "", "n1", "n2", "code", "EXTRACTED", "INFERRED",
          "document", "bad\n\t'\"", "é日本語\u2028\u200b\U000e0001"]
values += [chr(rng.choice([*range(0x100), 0x2028, 0x200b, 0x10ffff])) for _ in range(100)]
valid = {"nodes": [{"id": "n1", "label": "A", "file_type": "code", "source_file": "a.py"}],
         "edges": [{"source": "n1", "target": "n1", "relation": "calls", "confidence": "EXTRACTED", "source_file": "a.py"}]}

try:
    cases = [None, [], {}, {"nodes": [], "links": []}, {"nodes": [], "edges": None, "links": []}]
    for value in values:
        for field in ("id", "file_type", "label", "source_file"):
            case = copy.deepcopy(valid)
            case["nodes"][0][field] = value
            cases.append(case)
        for field in ("source", "target", "confidence"):
            case = copy.deepcopy(valid)
            case["edges"][0][field] = value
            cases.append(case)
    for _ in range(5000):
        case = copy.deepcopy(valid)
        for _ in range(rng.randrange(1, 7)):
            section = rng.choice(["nodes", "edges"])
            if rng.randrange(5) == 0:
                case[section] = rng.choice(values)
            elif isinstance(case[section], list) and case[section] and isinstance(case[section][0], dict):
                target = case[section][0]
                key = rng.choice([*target, "id", "extra"])
                if rng.randrange(4) == 0:
                    target.pop(key, None)
                else:
                    target[key] = copy.deepcopy(rng.choice(values))
        cases.append(case)
    for case in cases:
        try:
            validate(case)
        except TypeError:
            pass
    reference.validate_extraction = validate
    reference.assert_valid = assert_valid
    tests = upstream_tests("tests/test_validate.py")
finally:
    native.close()

native.receipt("graphify/validate.py",
               ["src/validate.rs", "src/python_compat.rs", "src/python_repr_data.rs"],
               differential_cases=len(cases), upstream_tests=tests,
               diagnostic_order="Error multisets match; missing-field order is stable in Rust versus Python set iteration.",
               test_scope="Validator functions are native. The one upstream build-canonicalization fixture uses the original unported builder.")
