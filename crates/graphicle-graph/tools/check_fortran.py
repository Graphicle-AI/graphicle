#!/usr/bin/env python3
"""Fortran extractor, actual preprocessing and native argument parity."""
import ast
import importlib.metadata
import os
import random
import tempfile
from pathlib import Path
from unittest.mock import patch
from parity import Native,SOURCE,upstream_tests
import graphify.extract as facade
import graphify.extractors.fortran as reference
import tree_sitter_fortran
from tree_sitter import Language

native=Native('fortran');original=reference.extract_fortran;cases=0;preprocessors=0
def extract(path):
    global cases
    native.use_working_directory(os.getcwd())
    expected=original(path);actual=native.result('extract.fortran',path=str(path));cases+=1
    if expected!=actual:native.fail(path=str(path),source=path.read_bytes().hex() if path.is_file() else None,expected=expected,actual=actual)
    return actual
try:
    selected=['tests/test_languages.py::'+n.name for n in ast.parse((SOURCE/'tests/test_languages.py').read_text()).body if isinstance(n,ast.FunctionDef) and n.name.startswith('test_fortran_')]
    with patch.object(reference,'extract_fortran',extract),patch.object(facade,'extract_fortran',extract):tests=upstream_tests(*selected)
    argument_tests=upstream_tests('tests/test_cpp_preprocess.py')
    rng=random.Random(2026092321)
    sources=['', 'program A\nuse B\ncall x()\nend program A',
      'module Geometry\ntype Point\nreal :: x\nend type Point\ncontains\nfunction Make(x) result(p)\ntype(Point) :: x,p\nend function\nend module',
      'function foo(x)\ntype(Thing)::x,foo\ncall foo(x)\nend function',
      'subroutine outer(x)\ntype(Foo)::x\ncall later(x)\ncontains\nsubroutine later(y)\ntype(Foo)::y\ncall external(y)\nend subroutine\nend subroutine',
      'module M\ncontains\nfunction f(x) result(y)\nreal::x,y\ny=g(x)\nend function\nfunction g(x)\nreal::x,g\ng=arr(x)\nend function\nend module']
    for _ in range(500):
        items=['use Utility','implicit none','type Thing\ninteger::value\nend type Thing','contains']
        for i in range(rng.randrange(1,7)):
            name=rng.choice(['foo','bar','work','make']);function=rng.choice([True,False])
            header=f'function {name}(x)'+rng.choice([' result(y)','']) if function else f'subroutine {name}(x)'
            body=rng.choices(['call foo(x)','call bar(x)','x=work(x)','x=arr(x)','call external(x)','type(Thing)::x,y','type(Other)::x','real::x'],k=rng.randrange(12))
            items.append(header+'\n'+'\n'.join(body)+'\nend '+('function' if function else 'subroutine'))
        text='module Main\n'+'\n'.join(items)+'\nend module Main'
        if rng.random()<.3:text=text.upper()
        if rng.random()<.2:text=text[:rng.randrange(len(text))]
        sources.append(text)
    with tempfile.TemporaryDirectory(prefix='graphify-fortran-') as tmp:
        root=Path(tmp);path=root/'sample.f90'
        for source in sources:path.write_text(source,encoding='utf-8');extract(path)
        for raw in [b'\xffprogram x\nend program',b'\xef\xbb\xbfsubroutine go\nend',b'\x00program x\nend']:
            path.write_bytes(raw);extract(path)
        for suffix in ['F','F90','F95','F03','F08','f90','f95','F2008']:
            capital=root/f'macros.{suffix}'
            capital.write_text('#define NAME shapes\n#ifdef NAME\nmodule NAME\ncontains\nsubroutine work\nend subroutine\nend module NAME\n#endif\n')
            extract(capital)
            expected=reference._cpp_preprocess(capital);actual=bytes(native.result('fortran.preprocess',path=str(capital)));preprocessors+=1
            if actual!=expected:native.fail(path=str(capital),expected=expected.hex(),actual=actual.hex())
        for source in ['#error fail\nprogram main\nend','#include "missing-private-fixture.h"\nprogram main\nend','']:
            capital=root/'fallback.F90';capital.write_text(source);extract(capital)
            expected=reference._cpp_preprocess(capital);actual=bytes(native.result('fortran.preprocess',path=str(capital)));preprocessors+=1
            if actual!=expected:native.fail(path=str(capital),expected=expected.hex(),actual=actual.hex())
        saved=os.getcwd()
        try:
            os.chdir(root)
            for name in ['-Ietc.F90','-include.F90','two spaces.F90']:
                path=Path(name);path.write_text('program main\nend program main');extract(path)
        finally:os.chdir(saved)
        extract(root/'missing.f90');extract(root/'missing.F90');extract(root)
finally:native.close()
grammar=Language(tree_sitter_fortran.language())
native.receipt('graphify/extractors/fortran.py',['src/extractors/fortran.rs','src/process.rs'],extraction_cases=cases,preprocess_comparisons=preprocessors,upstream_tests=tests,argument_tests=argument_tests,
    grammar={'python_package':importlib.metadata.version('tree-sitter-fortran'),'rust_crate':'=0.6.0','node_kinds':grammar.node_kind_count,'parse_states':grammar.parse_state_count,'abi':grammar.abi_version},
    comparisons='Exact definitions, casing, module/program/procedure scope, forward procedure calls versus array indexing, derived type signature edges, source-free stubs, imports, malformed bytes, missing inputs and actual cpp output/fallback.',
    adaptations=['Grammar compiled into native binary.','The unchanged upstream security tests run against Python; Rust independently compares actual preprocessing output and tests stdin-only cpp invocation, absolute/traversing/same-directory include stripping, sanitized fallback, timeout and pipe handling.'],
    inherited_limit='Preprocessor include directives are intentionally stripped; macro expansion is preserved where a functional cpp is available.')
