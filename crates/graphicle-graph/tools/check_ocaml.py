#!/usr/bin/env python3
"""Exact OCaml implementation/interface extraction with the original regression suite."""
import importlib.metadata
import random
import tempfile
from pathlib import Path
from unittest.mock import patch
from parity import Native,upstream_tests
import graphify.extract as facade
import graphify.extractors.ocaml as reference
import tree_sitter_ocaml
from tree_sitter import Language

native=Native('ocaml');original=reference.extract_ocaml;cases=0
def extract(path):
    global cases
    expected=original(path);actual=native.result('extract.ocaml',path=str(path));cases+=1
    if actual!=expected:native.fail(path=str(path),source=path.read_bytes().hex() if path.is_file() else None,expected=expected,actual=actual)
    return actual
try:
    with patch.object(reference,'extract_ocaml',extract),patch.object(facade,'extract_ocaml',extract):tests=upstream_tests('tests/test_ocaml.py')
    rng=random.Random(2026092319)
    sources=['','open Stdlib.List\nlet helper x = List.map f x',
             'let rec first x = second x and second x = first x',
             'let create x = let local = Reg_spec.create x in local\nlet run () = Scope.create ()',
             'module M = struct\nlet helper x = x\nend\nlet run () = M.helper 1',
             'module type X = sig\nval run : int -> int\ntype shape = Circle | Square\nend',
             'type shape = Circle of float | Square of int\nand wrapper = W of shape',
             'let top x = let local a = global a in local x\nlet global y = y',
             'module A = struct\nmodule Inner = struct\nlet helper x = x\nend\nend\nlet main() = A.Inner.helper 1',
             'let (a,b) = make ()\nlet () = print_endline "hello"',
             'module Name = Other.Name\nopen Name\nlet run x = Name.make x',
             'module M : S = struct\nlet x = f 2\nend',
             'let incomplete = (\nmodule A = struct']
    for _ in range(550):
        parts=[]
        for i in range(rng.randrange(1,7)):
            name=rng.choice(['create','run','helper','same']);expr=rng.choice(['helper x','Reg_spec.create x','Other.go x','M.helper x','let local = create x in local','print_int (run x)','x'])
            decl=f'let {name} x = {expr}'
            if rng.random()<.3:decl=f'module {rng.choice(["M","Other","A"])} = struct\n{decl}\nend'
            parts.append(decl)
        parts.insert(0,rng.choice(['open Stdlib','type kind = X | Y of int','module type S = sig\nval create : int -> int\nend']))
        text='\n'.join(parts)
        if rng.random()<.2:text=text[:rng.randrange(len(text))]
        sources.append(text)
    with tempfile.TemporaryDirectory(prefix='graphify-ocaml-') as tmp:
        root=Path(tmp)
        for suffix in ['ml','mli']:
            path=root/f'sample.{suffix}'
            for source in sources:path.write_text(source,encoding='utf-8');extract(path)
            for raw in [b'\xff let x = other 1',b'\xef\xbb\xbflet run() = call()',b'let x=\x00call 1']:
                path.write_bytes(raw);extract(path)
        for name in ['Résumé.ml','two spaces.mli','UPPER.MLI','dir\\name.ml']:
            path=root/name;path.write_text(sources[3]);extract(path)
        extract(root/'missing.ml');extract(root)
finally:native.close()
grammars={}
for name in ['language_ocaml','language_ocaml_interface']:
    l=Language(getattr(tree_sitter_ocaml,name)());grammars[name]={'node_kinds':l.node_kind_count,'parse_states':l.parse_state_count,'abi':l.abi_version}
native.receipt('graphify/extractors/ocaml.py',['src/extractors/ocaml.rs'],extraction_cases=cases,upstream_tests=tests,
    grammar={'python_package':importlib.metadata.version('tree-sitter-ocaml'),'rust_crate':'=0.25.0',**grammars},
    comparisons='Exact node/edge order, definitions, variant constructors, local bindings, forward references, module qualifiers, external stubs, implementation/interface grammar selection, malformed source and file errors.',
    adaptations=['The optional OCaml dependency is installed and version-pinned in both the oracle and native build; no tests are satisfied solely by matching a missing dependency error.'])
