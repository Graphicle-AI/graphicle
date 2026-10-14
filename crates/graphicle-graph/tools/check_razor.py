#!/usr/bin/env python3
"""Compare Razor directives, components, methods and attributed metadata."""
import ast
import random
import tempfile
from pathlib import Path
from parity import Native, SOURCE, upstream_tests
import graphify.extractors.razor as reference

native=Native("razor")
original=reference.extract_razor
checked=0

def extract(path):
    global checked
    checked+=1
    expected=original(path)
    actual=native.result("extract.razor",path=str(path))
    if actual!=expected:
        native.fail(path=str(path),source=path.read_text(errors="replace") if path.is_file() else None,expected=expected,actual=actual)
    return actual

try:
    reference.extract_razor=extract
    cases=[]
    for file in ["tests/test_dotnet.py","tests/test_languages.py"]:
        tree=ast.parse((SOURCE/file).read_text())
        cases += [file+"::"+node.name for node in tree.body if isinstance(node,ast.FunctionDef) and node.name.startswith("test_razor_")]
    tests=upstream_tests(*cases)
    rng=random.Random(20260923)
    directives=['@using Foo.Bar;','@using static A.B','@using Alias = Very.Long.Type;','@using = Bad','@using !!!',
                '@inject Namespace.Service<Item[]> instance','@inject Service instance','@inject [] invalid',
                '@inherits Component<A>','@model SomeModel','@page "/foo/{id}"', '<MyWidget />','<Div />','<A />',
                '@code { public void Run() {} private async Task<A> Other() { if(true) {} } }',
                '@code { public int MissingEnd() {','@code { public void Repeat() {} }',
                '@functions { public int Square(int x) { return x * x; } }','@functions{ protected string Greet() {} }',
                ' @using Ignored','@using 日本語.É;','@using Foo\x1f','<MyOther\x1f/>']
    with tempfile.TemporaryDirectory(prefix="graphify-razor-parity-") as directory:
        root=Path(directory)
        extract(root/'missing.razor')
        extract(root)
        path=root/'Page.razor'
        for _ in range(250):
            text=rng.choice(['\n','\r\n','\u2028']).join(rng.choices(directives,k=rng.randrange(1,30)))
            path.write_bytes(text.encode()+b'\xff')
            extract(path)
finally:
    native.close()

native.receipt("graphify/extractors/razor.py",["src/extractors/razor.rs"],extractions_checked=checked,upstream_tests=tests,
               comparisons="Complete ordered output, source attribution, namespace aliases, injected types, components, code blocks and metadata sanitization.",
               test_scope="Razor extraction and metadata sanitization are native. C# extraction and cross-file resolution in upstream integration fixtures are still Python.")
