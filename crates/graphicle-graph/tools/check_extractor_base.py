#!/usr/bin/env python3
"""Check shared extractor identities, built-ins and lossy byte spans."""
import random
from pathlib import Path
from types import SimpleNamespace
from parity import Native, upstream_tests
import graphify.extractors.base as reference

native = Native("extractor_base")
original_stem = reference._file_stem
original_read = reference._read_text
original_id = reference._make_id
rng = random.Random(20260923)


def stem(path):
    actual = native.result("extractor.base", paths=[str(path)])["stems"][0]
    expected = original_stem(path)
    if actual != expected:
        native.fail(path=str(path), expected=expected, actual=actual)
    return actual


def make_id(*parts):
    actual = native.result("ids", values=[], parts=[parts])["made"][0]
    expected = original_id(*parts)
    assert actual == expected, (parts, expected, actual)
    return actual


try:
    paths = ["", ".", "..", "/", "//", "///", "foo/", "a.", ".env", "..env", "...", "é/日本語.py",
             "docs/v1/api/README.md", "docs/v2/api/README.md", "//server/share/a.py", r"C:\src\a.py"]
    parts = [".", "..", "foo", "a.py", "a.", ".env", "...a", "日本語", "", "C:", "x.tar.gz"]
    paths += ["/".join(rng.choices(parts, k=rng.randrange(1, 10))) for _ in range(3000)]
    for path in paths:
        stem(Path(path))
    spans = []
    for source in ["é日本語".encode(), b"\xf0\x80\x80\x80", bytes(range(256))] + [rng.randbytes(rng.randrange(50)) for _ in range(1000)]:
        start, end = rng.randrange(len(source) + 10), rng.randrange(len(source) + 10)
        spans.append({"source":list(source),"start":start,"end":end})
    result = native.result("extractor.base", spans=spans)
    expected = [original_read(SimpleNamespace(start_byte=s["start"], end_byte=s["end"]), bytes(s["source"])) for s in spans]
    if result["texts"] != expected:
        native.fail(spans=spans, expected=expected, actual=result["texts"])
    assert result["builtins"] == sorted(reference._LANGUAGE_BUILTIN_GLOBALS)
    # _read_source_bytes / _read_source_text (0.9.78): UTF-8 kept, BOM'd
    # UTF-16/UTF-32 decoded, else cp1252 then latin-1, newlines folded.
    texts = ["class CaféOrder { void Pay() {} }\r\n", "a = 1\rb = 'é'\r\n\u2028x", "\ufeffa", "\x00ab", "日本語 😀", ""]
    samples = [t.encode(codec) for t in texts for codec in ("utf-8", "utf-8-sig", "utf-16", "utf-16-le", "utf-16-be", "utf-32", "utf-32-be")]
    samples += [b"\xfe\xff" + t.encode("utf-16-be") for t in texts] + [b"\x00\x00\xfe\xff" + t.encode("utf-32-be") for t in texts]
    samples += [t.encode("cp1252", errors="replace") for t in texts] + [b"Caf\x81\x8d\x8f\x90\x9d\xe9", bytes(range(256))]
    samples += [b"\xff\xfe" + b"\x00\xd8A\x00", b"\xff\xfeA", b"\xff\xfe\x00\x00\x00\x00\x11\x00", b"\xed\xa0\x80", b"\xf4\x90\x80\x80"]
    alphabet = [b"a", b"\r", b"\n", b"\x00", b"\xff", b"\xfe", b"\xef\xbb\xbf", b"\xc3\xa9", b"\x80", b"\x81", b"\xd8", b"\xdc", b"\xe9"]
    samples += [b"".join(rng.choices(alphabet, k=rng.randrange(12))) for _ in range(1500)]
    samples += [rng.choice([b"\xff\xfe", b"\xfe\xff", b"\xff\xfe\x00\x00", b"\x00\x00\xfe\xff"]) + rng.randbytes(rng.randrange(16)) for _ in range(1500)]
    import tempfile
    with tempfile.TemporaryDirectory(prefix="graphify-decode-") as tmp:
        for index, data in enumerate(samples):
            path = Path(tmp) / f"s{index}.src"
            path.write_bytes(data)
            want_bytes = reference._read_source_bytes(path, warn=False)
            got = native.result("extract.language", action="decode_source", bytes=list(data))
            if bytes(got["bytes"]) != want_bytes:
                native.fail(source=list(data), expected=list(want_bytes), actual=got)
            want_text = reference._read_source_text(path, warn=False)
            got_text = native.result("extract.language", action="read_source_text", path=str(path))
            if got_text != want_text:
                native.fail(source=list(data), expected=want_text, actual=got_text)
    reference._file_stem = stem
    reference._make_id = make_id
    tests = upstream_tests("tests/test_extraction_spec_ids.py")
finally:
    native.close()

native.receipt("graphify/extractors/base.py",
               ["src/extractors/base.rs", "src/extractors/builtins_data.rs", "src/path_syntax.rs"],
               path_cases=len(paths), byte_spans=len(spans), builtin_names=len(result["builtins"]),
               decoded_sources=len(samples),
               upstream_tests=tests,
               platform="POSIX host path semantics checked; original fixtures retain their language.")
