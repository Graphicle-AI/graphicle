#!/usr/bin/env python3
"""Exact Julia extractor comparisons and shared semantic-edge contracts."""
import ast
import importlib.metadata
import random
import tempfile
from pathlib import Path
from unittest.mock import patch
from parity import Native,SOURCE,upstream_tests
import graphify.extract as facade
import graphify.extractors.julia as reference
import graphify.extractors.engine as engine
import tree_sitter_julia
from tree_sitter import Language

native=Native("julia");original=reference.extract_julia;cases=0;helpers=0
def extract(path):
    global cases
    expected=original(path);actual=native.result("extract.julia",path=str(path));cases+=1
    if expected!=actual:native.fail(path=str(path),source=path.read_bytes().hex() if path.is_file() else None,expected=expected,actual=actual)
    return actual
try:
    selected=["tests/test_languages.py::"+n.name for n in ast.parse((SOURCE/"tests/test_languages.py").read_text()).body if isinstance(n,ast.FunctionDef) and n.name.startswith("test_julia_")]
    with patch.object(reference,"extract_julia",extract),patch.object(facade,"extract_julia",extract):tests=upstream_tests(*selected)
    rng=random.Random(2026092318)
    sources=['','using Base.Threads\nimport ..Sibling: other\nusing Foo: bar, baz',
             'module M\nabstract type Shape end\nstruct Box <: Shape\nx::Int\ny::Thing\nend\nend',
             'abstract type BaseShape end\nabstract type Shape <: BaseShape end\nstruct Box <: Shape end',
             'foo(x) = bar(x)\nbar(x) = foo(x)',
             'function main(x)\n first(x)\nobj.method(x)\nunknown(x)\nend\nfunction first(x)\nmain(x)\nend',
             'module A\nmodule B\nfunction go()\n other()\nend\nend\nend',
             'struct Generic{T}\nx::T\ny::Array{T}\nend',
             'function foo(x)\nfunction inner()\n deep()\nend\n outer()\nend',
             'function broken(\n x.\nend',
             'mutable struct Café <: Δ\n 名字::类型\nend']
    for _ in range(500):
        pieces=[]
        for i in range(rng.randrange(1,7)):
            name=rng.choice(['foo','bar','go','work'])
            body='\n'.join(rng.choices(['foo(x)','bar(x)','obj.go(x)','external(x)','x = bar(x)','unknown.method()'],k=rng.randrange(9)))
            pieces.append(f'function {name}(x)\n{body}\nend' if rng.random()<.7 else f'{name}(x) = obj.go(x)')
        pieces.insert(0,'using Base.Threads\nimport ..Sibling: other')
        pieces.insert(1,rng.choice(['struct Data\nitem::Int\nend','abstract type Data <: Other end','mutable struct Data <: Other\nx::Thing\nend']))
        text='\n'.join(pieces)
        if rng.random()<.5:text='module Main\n'+text+'\nend'
        if rng.random()<.2:text=text[:rng.randrange(len(text))]
        sources.append(text)
    with tempfile.TemporaryDirectory(prefix='graphify-julia-') as tmp:
        root=Path(tmp);path=root/'sample.jl'
        for source in sources:path.write_text(source,encoding='utf-8');extract(path)
        for raw in [b'\xff function go()\nend',b'\xef\xbb\xbffoo() = bar()',b'foo()=\x00other()']:
            path.write_bytes(raw);extract(path)
        for name in ['Résumé.jl','two spaces.jl','Σ.jl','dir\\name.jl']:
            named=root/name;named.write_text(sources[2]);extract(named)
        extract(root/'missing.jl');extract(root)
    for line in [None,0,1,-5,False,True,4.5,'','4','L','L98','004','Σ']:
        expected=engine._source_location(line);actual=native.result('engine.location',line=line);helpers+=1
        if expected!=actual:native.fail(line=line,expected=expected,actual=actual)
        for context in engine.REFERENCE_CONTEXTS:
            expected=engine._semantic_reference_edge('a','b',context,'file.jl',line)
            actual=native.result('engine.reference_edge',source='a',target='b',context=context,source_file='file.jl',line=line);helpers+=1
            if expected!=actual:native.fail(line=line,context=context,expected=expected,actual=actual)
    assert not native.call('engine.reference_edge',source='a',target='b',context='invalid',source_file='file.jl',line=1)['ok']
    for _ in range(500):
        name=''.join(rng.choices(list('a.Az0_')+['Σ','é','字','\n','\x00'],k=rng.randrange(40)))
        expected=engine._csharp_namespace_id(name);actual=native.result('engine.namespace_id',name=name);helpers+=1
        if expected!=actual:native.fail(name=name,expected=expected,actual=actual)
finally:native.close()
grammar=Language(tree_sitter_julia.language())
native.receipt('graphify/extractors/julia.py',['src/extractors/julia.rs','src/extractors/engine_semantics.rs'],extraction_cases=cases,helper_comparisons=helpers,upstream_tests=tests,
    grammar={'python_package':importlib.metadata.version('tree-sitter-julia'),'rust_crate':'=0.23.1','node_kinds':grammar.node_kind_count,'parse_states':grammar.parse_state_count,'abi':grammar.abi_version},
    comparisons='Exact modules, types, field references, inheritance, qualified/relative imports, function bodies, short forms, attributed calls, source bytes and failures.',
    dependencies='Three edge/namespace helpers from engine.py are native dependencies; this does not mark the generic extraction engine complete.',
    adaptations=['Pinned grammar compiled into the binary; optional Python import failures become native dependency/build validation.'])
