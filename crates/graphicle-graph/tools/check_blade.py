#!/usr/bin/env python3
"""Exercise every Blade extraction branch against the pinned complete function."""
import random
import tempfile
from pathlib import Path
from parity import Native, SOURCE, upstream_tests
import graphify.extract as pipeline
from graphify.extractors.blade import extract_blade

native = Native("blade")
checked = 0


def compare(path):
    global checked
    checked += 1
    expected = extract_blade(path)
    actual = native.result("extract.blade", path=str(path))
    if expected != actual:
        native.fail(path=str(path), text=path.read_text(errors="replace") if path.is_file() else None,
                    expected=expected, actual=actual)
    return actual


try:
    # The 0.9.72 tag's Blade test runs the whole extraction; its dispatcher
    # reads this module global, so every Blade file goes through compare().
    pipeline.extract_blade = compare
    tests = upstream_tests("tests/test_blade_extractor.py")
    rng = random.Random(20260923)
    with tempfile.TemporaryDirectory(prefix="graphify-blade-parity-") as directory:
        root = Path(directory)
        compare(root / "missing.blade.php")
        compare(root)
        path = root / "page.blade.php"
        names = ["", "foo.bar", "save()", "first-name", "true", "Foo", "___", "é日本語", "a\u0301", "a\u200d", "日本²", "new\nline", "quote'\""]
        for case in range(400):
            parts = []
            for _ in range(rng.randrange(1, 20)):
                name = rng.choice(names)
                quote = rng.choice(["'", '"'])
                parts.append(rng.choice([f"@include({quote}{name}{quote})", f"<livewire:{name} />", f"wire:click={quote}{name}{quote}", f"@include ({quote}{name}{quote})",
                                         f"@extends({quote}{name}{quote})", f"@extends ({quote}{name}{quote})"]))
            path.write_bytes("\r\n".join(parts).encode() + b"\xff")
            compare(path)
        for cp in [*range(256), 0x345, 0x200d, 0x2118, 0x2160, 0x2ebf0, 0x10ffff]:
            path.write_text(f"<livewire:a{chr(cp)}z />")
            compare(path)
    fixtures = list((SOURCE / "tests/fixtures").rglob("*.blade.php"))
    for path in fixtures:
        compare(path)
finally:
    pipeline.extract_blade = extract_blade
    native.close()

native.receipt("graphify/extractors/blade.py", ["src/extractors/blade.rs", "src/regex_compat.rs"],
               extractions_checked=checked, original_fixtures=len(fixtures),
               comparisons="Complete ordered extraction JSON, all four directive classes (@extends, @include, <livewire:>, wire:click), duplicate references, Unicode word boundaries, read failures.",
               upstream_tests=tests)
