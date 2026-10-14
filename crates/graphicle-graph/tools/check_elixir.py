#!/usr/bin/env python3
"""Check Elixir extraction against every applicable original language test."""
import ast
import importlib.metadata
import random
import tempfile
from pathlib import Path
from unittest.mock import patch
from parity import Native,SOURCE,upstream_tests
import graphify.extract as facade
import graphify.extractors.elixir as reference
import tree_sitter_elixir
from tree_sitter import Language

native=Native("elixir");original=reference.extract_elixir;cases=0
def extract(path):
    global cases
    expected=original(path);actual=native.result("extract.elixir",path=str(path));cases+=1
    if expected!=actual:native.fail(path=str(path),source=path.read_bytes().hex() if path.is_file() else None,expected=expected,actual=actual)
    return actual
try:
    selected=["tests/test_languages.py::"+n.name for n in ast.parse((SOURCE/"tests/test_languages.py").read_text()).body if isinstance(n,ast.FunctionDef) and n.name.startswith("test_elixir_")]
    # 0.9.81/0.9.85: keyword-form bodies, qualified calls through aliases (the
    # corpus resolver reads the native elixir_module) and defdelegate.
    selected+=["tests/test_elixir_keyword_def_calls.py","tests/test_elixir_defdelegate.py","tests/test_elixir_qualified_calls.py"]
    dispatch={k:extract for k,v in facade._DISPATCH.items() if v is original}
    with patch.object(reference,"extract_elixir",extract),patch.object(facade,"extract_elixir",extract),patch.dict(facade._DISPATCH,dispatch):tests=upstream_tests(*selected)
    rng=random.Random(2026092317)
    sources=['','defmodule Top do\n defmodule Inner do\n def inner(), do: 1\nend\nend',
             'def outer do\n helper()\nend\ndefp helper(), do: 1',
             'defmodule A do\n alias Foo.{Bar, Baz}\n import X.Y\n require One\n use Base\nend',
             'alias Foo.{}\nalias Foo.{Bar.Baz, Blah}\n',
             'defmodule A do\n def f(x) when is_integer(x) do\n f(x); helper()\n end\n def helper(), do: 1\nend',
             'def f, do: g()\ndefp g, do: 1',
             'defmodule A do\n def run() do\n if true do\n other()\nelse\n target.call()\nend\nend\nend',
             'defmacro rule(x), do: x\ndefmodule A do\n defstruct [:x]\nend',
             'defmodule A do\n def f(x) do\n fn y -> g(y) end\nend\n def g(y) do\n g(y)\nend\nend',
             'defmodule A do\n def go do\n parse("abc"); other(); other()\nend\nend',
             'defmodule Ω do\n def café() do\n π()\nend\nend']
    for _ in range(550):
        pieces=[]
        for i in range(rng.randrange(1,6)):
            body='\n'.join(rng.choices(['run()','help()','outside()','Mod.call()','if true do\n run()\n end','Integer.parse("2")','List.flatten([])','fn x -> help(x) end'],k=rng.randrange(8)))
            name=rng.choice(['run','help','factory','unused']);visibility=rng.choice(['def','defp'])
            guard=rng.choice(['',' when is_integer(x)',' when x > 0 and x < 20'])
            if rng.random()<.15:decl=f'{visibility} {name}(x){guard}, do: help(x)'
            else:decl=f'{visibility} {name}(x){guard} do\n{body}\nend'
            pieces.append(decl)
        text='\n'.join(pieces)
        if rng.random()<.8:text='defmodule '+rng.choice(['Top','Demo.App','A'])+' do\nalias Foo.{Bar, Baz}\n'+text+'\nend'
        if rng.random()<.2:text=text[:rng.randrange(len(text))]
        if rng.random()<.15:text=text.replace('\n','\r\n')
        sources.append(text)
    with tempfile.TemporaryDirectory(prefix="graphify-elixir-") as tmp:
        root=Path(tmp);path=root/'sample.ex'
        for text in sources:path.write_text(text,encoding='utf-8');extract(path)
        for raw in [b'\xff defmodule A do\nend',b'\xef\xbb\xbfdef test, do: 1',b'def f do\n\x00 z()\nend']:
            path.write_bytes(raw);extract(path)
        for name in ['résumé.ex','two spaces.exs','Σ.ex','dir\\name.ex']:
            named=root/name;named.write_text(sources[3]);extract(named)
        extract(root/'missing.ex');extract(root)
finally:native.close()
grammar=Language(tree_sitter_elixir.language())
native.receipt("graphify/extractors/elixir.py",["src/extractors/elixir.rs"],extraction_cases=cases,upstream_tests=tests,
    grammar={"python_package":importlib.metadata.version("tree-sitter-elixir"),"rust_crate":"=0.3.5","node_kinds":grammar.node_kind_count,"parse_states":grammar.parse_state_count,"abi":grammar.abi_version},
    comparisons="Exact nodes, edges, module markers, guarded heads, grouped aliases, imports, contexts, unresolved calls, duplicate labels/clauses, nested expressions and error output.",
    scope="This receipt covers the complete extractor file. Corpus-wide Elixir import resolution and incremental workflow orchestration are separate files still tracked on the ledger.",
    adaptations=["The pinned grammar is compiled into the native binary; optional Python-package import failure is replaced by native dependency/build validation."])
