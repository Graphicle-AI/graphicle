#!/usr/bin/env python3
"""Compare manifest graphs from the same on-disk inputs, without running Cargo."""
import json
import random
import tempfile
from pathlib import Path
from parity import Native, upstream_tests
import graphify.cargo_introspect as reference

native = Native("cargo_introspect")
original = reference.introspect_cargo
checked = 0


def introspect(root):
    global checked
    checked += 1
    actual = native.call("cargo.introspect", root=str(root))
    try:
        expected = original(root)
    except Exception as error:
        error_class = type(error).__name__
        if actual.get("ok") or not actual.get("error", "").startswith(error_class + ":"):
            if not (isinstance(error, OSError) and actual.get("error", "").startswith("OSError:")):
                native.fail(root=str(root), expected_type=error_class, expected=str(error), actual=actual)
        raise
    if not actual.get("ok") or expected != actual["result"]:
        native.fail(root=str(root), expected=expected, actual=actual)
    return actual["result"]


try:
    reference.introspect_cargo = introspect
    tests = upstream_tests("tests/test_cargo_introspect.py")
    rng = random.Random(20260923)
    with tempfile.TemporaryDirectory(prefix="graphify-cargo-parity-") as directory:
        for case in range(40):
            root = Path(directory) / f"workspace-{case}"
            root.mkdir()
            package_count = rng.randrange(1, 31)
            patterns = ["crates/*", "crates/hidden/*", "crates/*", "does-not-exist"]
            (root / "Cargo.toml").write_text('[package]\nname = "root"\nversion = "0.1.0"\n'
                + '[workspace]\nmembers = ' + json.dumps(patterns) + '\n')
            for i in range(package_count):
                name = f"crate_{i:03d}" if i % 3 else f"日本語_{i:03d}"
                member = root / "crates" / ("hidden" if i % 2 else ".hidden") / str(i) if i % 5 == 0 else root / "crates" / str(i)
                member.mkdir(parents=True)
                dependencies = '\n[dependencies]\nroot = { path = "../.." }\nserde = "1"\n'
                if i:
                    dependencies += f'other = {{ package = "crate_{i-1:03d}", version = "0" }}\n'
                (member / "Cargo.toml").write_text(f'[package]\nname = "{name}"\nversion = "0.1.0"\n' + dependencies)
            introspect(root)
finally:
    native.close()

native.receipt("graphify/cargo_introspect.py", ["src/cargo_introspect.rs"],
               workspaces_checked=checked, upstream_tests=tests,
               error_compatibility="Malformed TOML retains the TOMLDecodeError category; text comes from Rust's TOML parser.",
               adaptations=["Inherited workspace dependency paths are anchored at the discovered Cargo.toml directory even when the scan root is above it; covered by tests/compat_0_9_66_cargo.rs."],
               execution="Manifest reading and globbing only; no cargo subprocess or build script executed.")
