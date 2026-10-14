#!/usr/bin/env python3
"""Full ordered Markdown outputs compared with the pinned, explicitly adapted extractor."""
import ast
import os
import random
import tempfile
import types
from pathlib import Path
from parity import Native, SOURCE, upstream_tests
import naming
from upstream_fixes import oracle_source
import graphify.extractors.markdown as reference
from graphify.security import sanitize_metadata
import yaml

# The pinned extractor with the reviewed post-pin fixes applied in memory (PRs
# #3450, #3772 and #3826, including canonical ignore anchors); the pinned module
# object stays in place for facade dispatch.
oracle=types.ModuleType("graphify_oracle_markdown")
oracle.__package__="graphify.extractors"
exec(compile(oracle_source("graphify/extractors/markdown.py"),"<pinned markdown.py with reviewed fixes>","exec"),oracle.__dict__)

native=Native("markdown")
processes={os.getcwd():native}
original=oracle.extract_markdown
original_mention=oracle._code_span_mention
original_fallback=oracle._parse_frontmatter_fallback
original_frontmatter=oracle._parse_frontmatter
original_resolve=oracle._resolve_markdown_link
counts={"extractions":0,"mentions":0,"frontmatter":0,"links":0}

def inputs(path=None,root=None):
    """Fixture ignore files, read by both sides under their own names."""
    near=list(Path(path).resolve().parents) if path else []
    trees=[root] if root else []
    return naming.native_inputs(*near,*trees,recursive=trees)

def current():
    cwd=os.getcwd()
    if cwd not in processes: processes[cwd]=Native("markdown",cwd=cwd)
    return processes[cwd]

def extract(path):
    counts["extractions"]+=1
    expected=original(path)
    root=reference._active_scan_root()
    with inputs(path,root):actual=current().result("extract.markdown",path=str(path),scan_root=str(root) if root else None)
    if actual!=expected:
        native.fail(path=str(path),source=path.read_text(errors="replace") if path.is_file() else None,expected=expected,actual=actual)
    return actual

def mention(span):
    counts["mentions"]+=1
    expected=original_mention(span)
    actual=current().result("markdown.mention",span=span)
    if actual is not None: actual=(actual[0],actual[1])
    if actual!=expected: native.fail(span=span,expected=expected,actual=actual)
    return actual

def frontmatter(lines,fallback=False):
    counts["frontmatter"]+=1
    expected=sanitize_metadata((original_fallback if fallback else original_frontmatter)(lines))
    actual=current().result("markdown.frontmatter",lines=lines,fallback=fallback)
    if actual!=expected: native.fail(lines=lines,fallback=fallback,expected=expected,actual=actual)
    return actual

def fallback(lines):
    # Upstream function returns raw metadata; only compare sanitized output at
    # the shipped graph boundary, then preserve raw data for the Python oracle.
    frontmatter(lines,True)
    return original_fallback(lines)

class Cache(dict):
    def clear(self):
        super().clear()
        oracle._MD_LINK_INDEX_CACHE.clear()
        for process in processes.values(): process.result("markdown.clear_cache")

try:
    reference._MD_LINK_INDEX_CACHE=Cache()
    reference.extract_markdown=extract
    reference._code_span_mention=mention
    reference._parse_frontmatter_fallback=fallback
    oracle._code_span_mention=mention
    oracle._parse_frontmatter_fallback=fallback
    tree=ast.parse((SOURCE/"tests/test_languages.py").read_text())
    cases=["tests/test_languages.py::"+node.name for node in tree.body if isinstance(node,ast.FunctionDef) and node.name.startswith("test_markdown_")]
    tests=upstream_tests(*cases,"tests/test_markdown_code_spans.py")
    rng=random.Random(20260923)
    snippets=["# Title","## Title","# `Widget`","### Sub", "# A\x1fword", "`Widget` `Widget` `str()`", "`src/mod.rs::Widget::run()`", "``a`b`` `X`", "`pkg.Éclair`", "`Thing.foo` `Thing.rs`", "`a²` `a\u0301`", "[x](other.md)", "![img](other.md)","[[other]]","![[other]]", "[[folder/target|Nice]]","[x](<other.md> \"Title\")","[x]: other.md", "[go](https://example.com)","[up](../other.md)","```python","~~~","# same","####### seven", "[a](missing.md) [[missing]]", "[[#Title]]", "[[Page#Sub]]", "[[other#Other]]", "[[]]", "[[#missing]]", "| [[folder/target\\|Nice]] |", "[[#Title|alias]] [[Page#Title]]"]
    fms=["", "---\nstatus: active\n---\n", "---\n# comment\nflags: [yes, OFF, null]\n---\n", "---\nnested:\n  rows: [1, 2, 3]\n  page: '<script>'\n---\n", "---\naliases: ['[[other]]']\n...\n", "---\nvalue: [bad\n---\n", "---\n# unterminated\n"]
    with tempfile.TemporaryDirectory(prefix="graphify-markdown-parity-") as directory:
        root=Path(directory)
        (root/"other.md").write_text("# Other")
        (root/"folder").mkdir()
        (root/"folder/target.md").write_text("# Target")
        path=root/"Page.md"
        extract(root/"missing.md");extract(root)
        # PR #3450: same-page anchors become heading->heading references, even
        # when the heading is declared later; PR #3772: a table-escaped alias
        # pipe is not part of the wikilink target.
        (root/"notes.md").write_text("# Intro\nsee [[#Setup]] and [[#Setup]], [[notes#Setup]], [[other#Other]], [[#Missing]], [[]], [[|alias]]\n\n## Setup\n\n| [[folder/target\\|Alias]] |\n")
        notes=extract(root/"notes.md")
        nid={n["label"]:n["id"] for n in notes["nodes"]}
        refs=[(e["source"],e["target"]) for e in notes["edges"] if e["relation"]=="references"]
        assert len(refs)==3 and refs[-1]==(nid["Intro"],nid["Setup"]), refs
        assert refs[0][1].endswith("other_md") and refs[1][1].endswith("folder_target_md"), refs
        for _ in range(250):
            text=rng.choice(fms)+rng.choice(["\n","\r\n","\u2028"]).join(rng.choices(snippets,k=rng.randrange(1,30)))
            path.write_bytes(text.encode()+b'\xff')
            extract(path)
        for raw in ["", "#here", "?x=1", " a.md#here?foo ", "foo/../other", "..", "../../x.txt", "x.PY", "x.MD", "//a/b.md", "http://foo.md", "mailto:x", "data:x.md", "x\\other.md", "x.", "x/.hidden", ".hidden.md"]:
            for wiki in [False,True]:
                counts["links"]+=1
                expected=original_resolve(raw,root,wikilink=wiki)
                actual=current().result("markdown.resolve",raw=raw,dir=str(root),wikilink=wiki)
                if actual!=(str(expected) if expected is not None else None): native.fail(raw=raw,expected=str(expected),actual=actual)
        # PR #3826: ignored files cannot win the vault fallback. Include a
        # nested ignore file and a valid candidate so exclusion cannot pass
        # merely by disabling all fallback resolution.
        (root/'.graphifyignore').write_text('bigdata/\nprivate.md\n')
        (root/'bigdata').mkdir(); (root/'bigdata/secret.md').write_text('# Secret')
        (root/'folder/private.md').write_text('# Private')
        (root/'folder/.gitignore').write_text('hidden.md\n')
        (root/'folder/hidden.md').write_text('# Hidden')
        oracle._MD_LINK_INDEX_CACHE.clear()
        current().result('markdown.clear_cache')
        for name in ('target.md', 'secret.md', 'private.md', 'hidden.md'):
            expected=oracle._vault_lookup(name, root)
            with inputs(root/'missing'/name,root):actual=current().result('markdown.resolve',raw=name,dir=str(root/'missing'),wikilink=True,scan_root=str(root))
            expected=str(expected if expected is not None else root/'missing'/name)
            if actual != expected:native.fail(raw=name,expected=expected,actual=actual)
            counts['links']+=1
    for scalar in ["yes","YES","yEs","off","n","null","~","0123","0b101","0xDe_ad","-1_234","1:23","1:23:12.5","1.0e+3","1e3","1.0e3","1.3","-0.0","2026-09-23","2026-9-3T1:02:03.12Z","2026-9-3","2026-02-29","2024-02-29","2026-09-23 01:02:03-3:30","'yes'","\"0123\"","!!str 0123","!!int '0123'","!!binary SGVsbG8K", "!unknown xyz"]:
        frontmatter(["value: "+scalar])
    for text in ["a: 1\na: 2", "a: &name [1, 2]\nb: *name", "base: &b {x: 1, y: 2}\nother: {<<: *b, y: 3}", "a: &a {x: 1}\nb: &b {x: 2, y: 3}\nc: {<<: [*a, *b]}", "'<<': plain", "true: first\n1: second\nfalse: third\n0: fourth", "map: {1: one, false: no, null: value}", "array:\n - yes\n - [1, 2]\n - {hello: world}", "text: |\n  hello\n  world\n", "text: >-\n  hello\n  world", "value: !!omap [{a: one}, {b: two}]", "value: !!set {a: null}", "[one, two]", "a: [malformed", "a: &x foo\nb: &x bar\nc: *x"]:
        frontmatter(text.splitlines())
finally:
    for process in processes.values(): process.close()

native.calls=sum(process.calls for process in processes.values())
native.receipt("graphify/extractors/markdown.py",["src/extractors/markdown.rs","src/extractors/markdown_data.rs","src/extractors/yaml_frontmatter.rs"],
    checked=counts,upstream_tests=tests,pyyaml_version=yaml.__version__,
    comparisons="Ordered nodes/edges/raw calls, heading ownership, frontmatter, vault fallback, inline/reference links, symbol mentions, source attribution and malformed-input handling.",
    test_scope="Markdown, IDs and metadata sanitization run natively. Upstream orchestration and other-language symbol resolution in integration fixtures remain Python.",
    native_adaptations=["Native YAML is always available. Python without optional PyYAML falls back to flat strings; the native flat fallback API is separately verified.","Non-finite/out-of-range YAML numbers and cyclic/excessively deep aliases use the flat fallback because the native interface requires JSON values.","YAML sets use deterministic sorted representation rather than Python process-dependent set order.","Native regex execution errors propagate instead of silently dropping edges."])
