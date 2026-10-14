#!/usr/bin/env python3
"""Freeze Unicode data used by the pinned CPython oracle; never run at app startup."""
import hashlib
import json
import unicodedata
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
if unicodedata.unidata_version != "15.0.0":
    raise SystemExit("Use the pinned Python 3.12 oracle (Unicode 15.0.0)")


def escaped(value):
    return "".join("\\u{" + format(ord(c), "x") + "}" for c in value)


folds, ranges = [], []
start = None
for cp in range(0x110000):
    c = chr(cp)
    if c.casefold() != c:
        folds.append((cp, c.casefold()))
    word = c.isalnum() or c == "_"
    if word and start is None:
        start = cp
    elif not word and start is not None:
        ranges.append((start, cp - 1))
        start = None
if start is not None:
    ranges.append((start, 0x10FFFF))


def code_point_ranges(predicate):
    """Maximal inclusive code point ranges for which ``predicate`` holds."""
    out, first = [], None
    for cp in range(0x110000):
        if predicate(chr(cp)):
            if first is None:
                first = cp
        elif first is not None:
            out.append((first, cp - 1))
            first = None
    if first is not None:
        out.append((first, 0x10FFFF))
    return out


# str.isidentifier(): the first character must be XID_Start or "_", the rest
# XID_Continue. Membership is per character, so each table is probed alone.
identifier_start = code_point_ranges(str.isidentifier)
identifier_continue = code_point_ranges(lambda c: ("a" + c).isidentifier())
# Combining marks (general category M*): dedup's label normalization keeps them
# inside a word next to str.isalnum() characters (Graphify 0.9.86, #4317).
marks = code_point_ranges(lambda c: unicodedata.category(c).startswith("M"))
lines = ["// Generated Unicode 15.0.0 data; see tools/generate_unicode.py and NOTICE.md.", "#[rustfmt::skip]", "pub const CASEFOLD: &[(char, &str)] = &["]
lines.extend("    ('\\u{" + format(cp, "x") + "}', \"" + escaped(value) + "\")," for cp, value in folds)
lines.extend(["];", "#[rustfmt::skip]", "pub const WORD_RANGES: &[(u32, u32)] = &["])
lines.extend(f"    (0x{a:x}, 0x{b:x})," for a, b in ranges)
lines.append("];")
for name, table in (("IDENTIFIER_START", identifier_start), ("IDENTIFIER_CONTINUE", identifier_continue), ("MARK_RANGES", marks)):
    lines.extend(["#[rustfmt::skip]", f"pub const {name}: &[(u32, u32)] = &["])
    lines.extend(f"    (0x{a:x}, 0x{b:x})," for a, b in table)
    lines.append("];")
path = ROOT / "src/unicode15.rs"
path.write_text("\n".join(lines) + "\n")
(ROOT / "migration/UNICODE.json").write_text(json.dumps({"unicode_version": unicodedata.unidata_version, "casefold_entries":len(folds), "word_ranges":len(ranges), "identifier_start_ranges":len(identifier_start), "identifier_continue_ranges":len(identifier_continue), "mark_ranges":len(marks), "generated_sha256":hashlib.sha256(path.read_bytes()).hexdigest(), "generator_sha256":hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), "normalization_dependency":"unicode-normalization = 0.1.22 (Unicode 15.0.0)"}, indent=2) + "\n")
print(f"Generated {len(folds)} case folds, {len(ranges)} Python word-character ranges and {len(identifier_start)}/{len(identifier_continue)} identifier ranges")

printable, start = [], None
for cp in range(0x110000):
    matches = chr(cp).isprintable()
    if matches and start is None:
        start = cp
    elif not matches and start is not None:
        printable.append((start, cp - 1))
        start = None
if start is not None:
    printable.append((start, 0x10FFFF))
lines = ["// Generated Unicode 15.0.0 printable-character data for Python-compatible diagnostics.", "#[rustfmt::skip]", "pub const PRINTABLE: &[(u32, u32)] = &["]
lines.extend(f"    (0x{a:x}, 0x{b:x})," for a, b in printable)
lines.append("];")
(ROOT / "src/python_repr_data.rs").write_text("\n".join(lines) + "\n")
