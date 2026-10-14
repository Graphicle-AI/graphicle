#!/usr/bin/env python3
"""Go extraction and original multi-file resolution/collision regression fixtures."""
import ast
import importlib.metadata
import os
import random
import tempfile
from pathlib import Path
from unittest.mock import patch
from parity import Native,SOURCE,upstream_tests
import graphify.extract as facade
import graphify.extractors.go as reference
import tree_sitter_go
from tree_sitter import Language

native=Native('go');original=reference.extract_go;cases=0
def extract(path):
    global cases
    native.use_working_directory(os.getcwd())
    expected=original(path);actual=native.result('extract.go',path=str(path));cases+=1
    if expected!=actual:native.fail(path=str(path),source=path.read_bytes().hex() if path.is_file() else None,expected=expected,actual=actual)
    return actual
try:
    selected=[]
    for file in ['tests/test_languages.py','tests/test_multilang.py','tests/test_extract.py']:
        selected += [file+'::'+n.name for n in ast.parse((SOURCE/file).read_text()).body if isinstance(n,ast.FunctionDef) and n.name.startswith('test_go_') and n.name!='test_go_method_declaration_emits_refs_only_when_name_present']
    selected += ['tests/test_go_interface_methods.py','tests/test_go_qualified_resolution.py']
    selected += ['tests/test_go_generic_type_params.py']  # 0.9.88, #4383
    selected += ['tests/test_go_builtin_call_targets.py::'+n.name for n in ast.parse((SOURCE/'tests/test_go_builtin_call_targets.py').read_text()).body if isinstance(n,ast.FunctionDef) and n.name.startswith('test_') and not n.name.startswith('test_rust_')]
    dispatch={k:extract for k,v in facade._DISPATCH.items() if v is original}
    with patch.object(reference,'extract_go',extract),patch.object(facade,'extract_go',extract),patch.dict(facade._DISPATCH,dispatch):tests=upstream_tests(*selected)
    rng=random.Random(2026092324)
    sources=['','package p\nfunc Run(){ run(); RUN() }\nfunc run(){}\nfunc RUN(){}',
       'package p\nimport maker "example.com/pkg"\nimport "testing"\nfunc New(){}\nfunc F(x *testing.T) *maker.Thing { maker.New(); New(); return nil }',
       'package p\ntype Base struct{}\ntype Box[T any] struct{ Item T }\ntype Item struct{ Base; value Box[Other]; Map map[Key][]*Value }',
       'package p\ntype A interface{ Read(x []byte)(int,error) }\ntype B interface{ A; ~C | D; Close() error }',
       'package p\nfunc (s *Store) Get() int{return s.get()}\nfunc (s Store) get() int{return 0}\nfunc (s Store) append(x int){}\nfunc F(s Store){append(nil,1);s.append(1)}',
       'package p\nimport (`fmt`; _ "unused"; . "dot"; alias "path/last")\nfunc F(){fmt.Println();alias.X();external.X();obj.method()}',
       'package p\nfunc Café(x Box[Thing]) (a Other, b *Obj) { return Other{},nil }\nfunc café(){}',
       'package p\nfunc Σ(){}\nfunc σ(){}\nfunc ς(){}\nfunc F(){Σ();σ();ς()}']
    for _ in range(350):
        pieces=['package sample','import ("fmt"; other "example.com/pkg"; _ "hidden")',
          'type Thing struct{}','type Item struct { Thing; config Config; values []Box[Payload] }',
          'type Contract interface { Base; Read([]byte) (int,error); ~Thing | Other }']
        for _ in range(rng.randrange(1,8)):
            name=rng.choice(['Run','run','RUN','Get','get','append','work','Build'])
            body='; '.join(rng.choices(['Run()','run()','RUN()','work()','other.Run()','fmt.Println(1)','x.Get()','append(nil,1)','len(nil)','unknown()'],k=rng.randrange(8)))
            header=f'func {name}(x *Item, y Box[Payload]) *Result' if rng.random()<.65 else f'func (s *Item) {name}(x Arg) Result'
            pieces.append(header+' { '+body+'; return nil }')
        text='\n'.join(pieces)
        if rng.random()<.2:text=text[:rng.randrange(len(text))]
        sources.append(text)
    with tempfile.TemporaryDirectory(prefix='graphify-go-') as tmp:
        root=Path(tmp);path=root/'sample.go'
        for text in sources:path.write_text(text,encoding='utf-8');extract(path)
        for builtin in reference._GO_PREDECLARED_FUNCS:
            path.write_text(f'package p\nfunc (s Item) {builtin}(){{}}\nfunc F(s Item){{{builtin}();s.{builtin}()}}');extract(path)
        for raw in [b'\xffpackage p\nfunc F(){}',b'\xef\xbb\xbfpackage p\nfunc X(){}',b'package p\nfunc F(){\x00}']:
            path.write_bytes(raw);extract(path)
        saved=os.getcwd()
        try:
            os.chdir(root);extract(Path('sample.go'));extract(Path('./sample.go'))
            (root/'src').mkdir();(root/'src/worker.go').write_text(sources[5]);extract(Path('src/worker.go'));extract(Path('src/../sample.go'))
        finally:os.chdir(saved)
        extract(root/'absent.go');extract(root)
finally:native.close()
grammar=Language(tree_sitter_go.language())
native.receipt('graphify/extractors/go.py',['src/extractors/go.rs'],extraction_cases=cases,upstream_tests=tests,
    grammar={'python_package':importlib.metadata.version('tree-sitter-go'),'rust_crate':'=0.25.0','node_kinds':grammar.node_kind_count,'parse_states':grammar.parse_state_count,'abi':grammar.abi_version},
    comparisons='Exact package-scoped receivers, exported/unexported collision IDs, stable exported identity, generic signatures and fields, interfaces and type-set constraints, qualified type stubs, import aliases, builtin filtering, call/raw-call attribution and invalid inputs.',
    adaptations=['Original multi-file tests use native Go extraction with Python corpus resolution/build/cache still in place; those larger modules remain independently pending.','One test that introspects Python AST source is excluded from Rust behavior qualification; the Rust-only builtin guard is qualified with the Rust extractor port.'])
