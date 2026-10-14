#!/usr/bin/env python3
"""Exact Zig extraction comparisons, including the original enum/union regression."""
import importlib.metadata
import random
import tempfile
from pathlib import Path
from unittest.mock import patch
from parity import Native,SOURCE,upstream_tests
import graphify.extract as facade
import graphify.extractors.zig as reference
import tree_sitter_zig
from tree_sitter import Language

native=Native("zig");original=reference.extract_zig;cases=0
def extract(path):
    global cases
    expected=original(path);actual=native.result("extract.zig",path=str(path));cases+=1
    if actual!=expected:native.fail(path=str(path),source=path.read_bytes().hex() if path.is_file() else None,expected=expected,actual=actual)
    return actual
try:
    with patch.object(reference,"extract_zig",extract),patch.object(facade,"extract_zig",extract):
        tests=upstream_tests("tests/test_languages.py::test_zig_enum_and_union_methods_are_extracted",
                             "tests/test_languages.py::test_zig_enum_members_emit_case_of_nodes")
    extract(SOURCE/"tests/fixtures/sample.zig")
    with tempfile.TemporaryDirectory(prefix="graphify-zig-") as tmp:
        root=Path(tmp);path=root/"sample.zig"
        cases_text=["", "const std = @import(\"std\");",'const fs = @import("std").fs;', 'const x = @import("lib/worker.zig");',
                    'const c = @cImport({ @cInclude("stdio.h"); });',
                    'fn helper() void {} fn caller() void { helper(); helper(); }',
                    'fn recursive() void { recursive(); unknown(); object.unknown(); }',
                    'const Box = struct { fn run(self: *@This()) void { helper(); } const Nested = struct { fn test() void {} }; }; fn helper() void {}',
                    'const A = struct {fn go() void {}}; const B = struct {fn go() void {}}; fn c() void { a.go(); b.go(); }',
                    'const color = enum {red, blue, pub fn show(self: @This()) void { log(); }};',
                    'const shape = union(enum) {a: u8, b: f32, fn draw(self: @This()) void { paint(); }};',
                    '// fn fake() void {}\nconst text="fn fake() void {}";',
                    'test "anonymous" { helper(); } fn helper() void {}',
                    'pub fn broken( { invalid.; const E = enum {x, fn f() {} };',
                    'const λ = struct { fn @"Σ"() void { @"call-name"(); } };',
                    # Enum members become case_of nodes; struct/union fields stay untouched (0.9.73).
                    'const Dir = enum(u8) { north = 0, south, east = 4, west };',
                    'const Twice = enum { a, a, b }; const Other = enum { a, c };',
                    'const S = struct { x: u8, y: u16 = 1 }; const U = union(enum) { a: u8, b, c: f32 };',
                    'const Outer = struct { const Inner = enum { one, two }; fn go() void {} };',
                    'fn f() void { const Local = enum { x, y }; _ = Local; }',
                    'const Open = enum(u4) { ok, bad, _ }; const Quoted = enum { @"weird name", ok, pub fn show() void {} };',
                    'const Tagged = union(Kind) { a: u8 }; const Kind = enum { a, };',
                    'pub fn broken() void { const E = enum { x, y,']
        rng=random.Random(2026092315)
        for _ in range(400):
            declarations=[]
            for i in range(rng.randrange(1,8)):
                name=rng.choice(["a","b","run","make","repeated"])
                calls=' '.join(rng.choices(['a();','b();','run();','obj.make();','external();','@sizeOf(u32);'],k=rng.randrange(9)))
                decl=f'fn {name}() void {{ {calls} }}'
                if rng.random()<.5:
                    kind=rng.choice(['struct','enum','union(enum)','enum(u8)'])
                    members=rng.sample(['red','blue','a','run','green'],rng.randrange(4))
                    if kind.startswith('enum'):fields=''.join(m+rng.choice(['',' = '+str(rng.randrange(9))])+', ' for m in members)
                    else:fields=''.join(m+rng.choice([': u8',': f32',''])+', ' for m in members)
                    decl='const T'+str(i)+' = '+kind+' { '+fields+decl+' };'
                declarations.append(decl)
            declarations.insert(0,'const lib = @import("some/library.zig");')
            text='\n'.join(declarations)
            if rng.random()<.2:text=text[:rng.randrange(len(text))]
            if rng.random()<.2:text=text.replace('\n','\r\n')
            cases_text.append(text)
        for text in cases_text:path.write_text(text,encoding="utf-8");extract(path)
        for raw in [b'\xff fn foo() void {}',b'fn invalid() void {\x00 b();}',b'\xef\xbb\xbffn bom() void {}']:
            path.write_bytes(raw);extract(path)
        for name in ['RésuMé.zig','two words.zig','Σ.zig','...zig','dir\\name.zig']:
            named=root/name;named.write_text('fn hello() void {}');extract(named)
        extract(root/"absent.zig");extract(root)
finally:native.close()
grammar=Language(tree_sitter_zig.language())
native.receipt("graphify/extractors/zig.py",["src/extractors/zig.rs"],extraction_cases=cases,upstream_tests=tests,
    grammar={"python_package":importlib.metadata.version("tree-sitter-zig"),"rust_crate":"=1.1.2","node_kinds":grammar.node_kind_count,"parse_states":grammar.parse_state_count,"abi":grammar.abi_version},
    comparisons="Exact nodes, edges, source locations, unresolved calls and errors; functions, containers, enum members (case_of), methods, duplicate definitions/calls, built-in imports, incomplete syntax, invalid UTF-8 and paths.",
    adaptations=["The pinned Zig grammar is linked into the native binary; Python ImportError branches become dependency/build failures instead of optional Python-package errors."])
