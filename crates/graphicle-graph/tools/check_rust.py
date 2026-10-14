#!/usr/bin/env python3
"""Rust extractor parity, including generic-impl regression fixtures."""
import ast
import importlib.metadata
import os
import random
import tempfile
import types
from pathlib import Path
from unittest.mock import patch
from parity import Native,ROOT,SOURCE,upstream_tests
from upstream_fixes import oracle_source
import graphify.extract as facade
import graphify.extractors.rust as reference
import tree_sitter_rust
from tree_sitter import Language

# Pinned extractor with the reviewed post-pin fix (PR #2963) applied in memory;
# the pinned module object stays in place for facade dispatch identity.
oracle=types.ModuleType('graphify_oracle_rust');oracle.__package__='graphify.extractors'
exec(compile(oracle_source('graphify/extractors/rust.py'),'<pinned rust.py with reviewed fixes>','exec'),oracle.__dict__)

native=Native('rust');original=oracle.extract_rust;cases=0
def extract(path):
    global cases
    native.use_working_directory(os.getcwd())
    expected=original(path);actual=native.result('extract.rust',path=str(path));cases+=1
    if expected!=actual:native.fail(path=str(path),source=path.read_bytes().hex() if path.is_file() else None,expected=expected,actual=actual)
    return actual
try:
    selected=['tests/test_multilang.py::'+n.name for n in ast.parse((SOURCE/'tests/test_multilang.py').read_text()).body if isinstance(n,ast.FunctionDef) and n.name.startswith('test_rust_')]
    selected+=['tests/test_rust_self_member_calls.py::'+n.name for n in ast.parse((SOURCE/'tests/test_rust_self_member_calls.py').read_text()).body if isinstance(n,ast.FunctionDef) and n.name.startswith('test_') and not n.name.startswith('test_resolver_')]
    selected+=['tests/test_go_builtin_call_targets.py::test_rust_in_file_type_new_edge_survives']
    selected+=['tests/test_rust_anonymous_const.py']
    # 0.9.82/0.9.86: in-file call binding and grouped `use` bindings. Two use-tree
    # tests replace the Python _rust_use_bindings, which the native does not call.
    selected+=['tests/test_rust_in_file_call_binding.py']
    selected+=['tests/test_rust_use_trees.py::'+n.name for n in ast.parse((SOURCE/'tests/test_rust_use_trees.py').read_text()).body if isinstance(n,ast.FunctionDef) and n.name.startswith('test_') and n.name not in ('test_calls_are_the_ones_v8_finds','test_a_cache_entry_from_before_the_bindings_is_not_served')]
    # The prelude-type filter: no prelude nodes, a locally defined prelude name kept.
    selected+=['tests/test_rust_builtin_type_hubs.py']
    # macro_rules! definitions and invocations, first run here on 14 October 2026.
    selected+=['tests/test_rust_macro_rules.py']
    dispatch={k:extract for k,v in facade._DISPATCH.items() if v is reference.extract_rust}
    with patch.object(reference,'extract_rust',extract),patch.object(facade,'extract_rust',extract),patch.dict(facade._DISPATCH,dispatch):tests=upstream_tests(*selected)
    rng=random.Random(2026092325)
    sources=['','use crate::thing::{Foo,Bar};\nstruct Item { thing: Foo, nested: Option<Vec<Bar>> }',
       'trait Base {}\ntrait Contract: Base + Other<Payload> { fn required(&self, x: Input) -> Output; fn defaulted(&self){ external(); } }',
       'struct Wrapper(pub Logger, Config, [Thing; 3]); enum Event { Click(Logger), Move{size: Dim}, Empty, Nested(Option<Box<Item>>) }',
       'static STATE: Mutex<Item> = todo!(); const SIZE: usize = 3; impl Item { const OTHER: Thing = todo!(); fn new()->Self{todo!()} fn go(&self){Self::new();self.other();outside();} }',
       'mod a {pub struct Bucket<T>(pub T);} mod b {pub struct Bucket<T>(pub T);} impl<T> Bucket<T>{fn read(&self){self.unknown();}}',
       'struct Bucket<T>(T); impl<T> Bucket<T>{fn first(&self){self.fetch_value();}} impl<T:Clone> Bucket<T>{fn second(&self){self.fetch_value();}}',
       'struct Café; fn work(x: &Café)->crate::Other {unknown()}',
       # PR #2963: `Self` is a keyword, not a type reference; scoped `Self::Item` and `SelfLike` stay.
       'struct P; impl P { fn new() -> Self { P } fn scaled(&self) -> Self { P } fn items(&self) -> Vec<Self> { vec![] } fn item(&self) -> Self::Item { todo!() } fn like(&self) -> SelfLike { todo!() } }',
       # Enum variants become case_of nodes (0.9.73).
       'enum Shape { Circle, Square = 4, #[default] Empty, Circle }\nenum Other { Circle(Shape), Tri { side: Len } }\nenum Never {}',
       'mod geo { pub enum Dir<T> where T: Copy { North(T), South, } }\nfn f() { enum Local { A, B } }\nenum Café { Ω, r#type }',
       'enum Broken { A, B(',
       # Prelude names: only a declared struct/enum/trait shadows one in type
       # references (local_type_names); aliases and unions do not, calls use every local type.
       'pub type Result<T> = std::result::Result<T, Error>;\npub struct Error;\nfn f() -> Result<i32> { Result::Ok(1) }\nfn g(x: Result<String>, y: &Error) -> Vec<Result<u8>> { todo!() }',
       'type Option = u8;\nunion Box { a: u8 }\nfn h(x: Option, y: Box) -> HashMap<Option, Box> { Option::new(); Box::new(); todo!() }',
       'struct Result<T>(T);\ntype Vec = Result<u8>;\nfn f() -> Result<i32> { todo!() }\nfn g() -> Vec { Result(1) }',
       'enum Result { A }\nmod inner { pub type Result<T> = core::result::Result<T, ()>; fn k() -> Result<u8> { todo!() } }\nfn j(r: Result) -> Option<Result> { None }',
       'trait Error {}\ntype Error = u8;\nfn e(x: &dyn Error) -> Result<(), Box<dyn Error>> { todo!() }']
    impls=['impl<T> Bucket<T>','impl<U> Bucket<U>','impl<T:Clone> Bucket<T>','impl<T> Bucket<T> where T: Clone','impl<T> Trait for Bucket<T>',"impl<'a,T> Bucket<'a,T>",'impl<const N:usize> Bucket<N>','impl<T> Bucket<Vec<T>>','impl Bucket<Thing>','impl<A,B> Pair<A,B>','impl<A,B> Pair<B,A>','impl<A> Pair<A,A>','impl<T,T> Pair<T,T>','impl<T> other::Bucket<T>']
    for header in impls:sources.append('struct Bucket<T>(T);\n'+header+' { fn work(&self, x: &Input)->Output{ self.fetch_value(); external(); todo!() } }')
    sources+=['const _: () = assert!(true);\nconst _: fn() = || {};\nconst LIMIT: usize = 3;\nfn main(){ helper(); }\nfn helper(){}','static _X: u8 = 0;\nconst _: u8 = { fn inner(){} 0 };\nmod m { const _: () = (); }']
    for _ in range(320):
        pieces=rng.choices(['struct Item { value: Thing }','enum Event { A(Thing), B{item: Box<Other>} }','trait Trait: Base {fn required(&self)->Value; fn defaulted(&self){unknown();}}','use crate::module::*;','const VALUE: Option<Thing> = None;','struct Bucket<T>(T);'],k=rng.randrange(1,7))
        for _ in range(rng.randrange(1,5)):
            header=rng.choice(impls+['impl Item'])
            methods=[]
            for i in range(rng.randrange(1,4)):
                name=rng.choice(['work','fetch_value','new','show','other'])
                body='; '.join(rng.choices(['self.fetch_value()','self.unknown()','Other::new()','work()','outside()','obj.method()','self.clone()'],k=rng.randrange(8)))
                methods.append(f'fn {name}(&self,x: &Input) -> Option<Box<Thing>> {{ {body}; None }}')
            pieces.append(header+' { '+' '.join(methods)+' }')
        text='\n'.join(pieces)
        if rng.random()<.2:text=text[:rng.randrange(len(text))]
        sources.append(text)
    with tempfile.TemporaryDirectory(prefix='graphify-rust-') as tmp:
        root=Path(tmp);path=root/'sample.rs'
        for text in sources:path.write_text(text,encoding='utf-8');extract(path)
        for raw in [b'\xffstruct Item; fn F(){}',b'\xef\xbb\xbfstruct Item;',b'fn work(){\x00}']:
            path.write_bytes(raw);extract(path)
        saved=os.getcwd()
        try:os.chdir(root);extract(Path('sample.rs'));extract(Path('./sample.rs'))
        finally:os.chdir(saved)
        extract(root/'absent.rs');extract(root)
    # Real code files supplement the small fixtures and generated syntax cases.
    # lib.rs and llm.rs declare `pub type Result<T>` aliases of the prelude name.
    for file in ['src/graph.rs','src/global_graph.rs','src/extractors/rust.rs','src/extractors/go.rs','src/security.rs','src/lib.rs','src/llm.rs','src/model.rs']:extract(ROOT/file)
finally:native.close()
grammar=Language(tree_sitter_rust.language())
native.receipt('graphify/extractors/rust.py',['src/extractors/rust.rs'],extraction_cases=cases,upstream_tests=tests,
    grammar={'python_package':importlib.metadata.version('tree-sitter-rust'),'rust_crate':'=0.24.2','node_kinds':grammar.node_kind_count,'parse_states':grammar.parse_state_count,'abi':grammar.abi_version},
    comparisons='Exact structs/enums/tuple payloads, enum variants (case_of), traits and required/default methods, constants/statics, signatures, declaration counts, generic impl markers and conflicts, self-call provenance, scoped-call filtering, imports, real Rust source and malformed/missing inputs.',
    adaptations=['Native Rust extraction is substituted into the original multi-file/cache fixtures; their Python orchestration and resolver are not counted as ported here. Two resolver-only tests that do not invoke this extractor are excluded.','Grammar is compiled into the native binary.'])
