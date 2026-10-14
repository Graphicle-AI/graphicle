#!/usr/bin/env python3
"""Verilog/SystemVerilog extraction and semantic helper parity."""
import ast
import importlib.metadata
import random
import tempfile
from pathlib import Path
from unittest.mock import patch
from parity import Native,SOURCE,upstream_tests
import graphify.extract as facade
import graphify.extractors.verilog as reference
import tree_sitter_verilog
from tree_sitter import Language

native=Native('verilog');original=reference.extract_verilog;cases=0;helpers=0
def extract(path):
    global cases
    expected=original(path);actual=native.result('extract.verilog',path=str(path));cases+=1
    if expected!=actual:native.fail(path=str(path),source=path.read_bytes().hex() if path.is_file() else None,expected=expected,actual=actual)
    return actual
try:
    selected=['tests/test_languages.py::'+n.name for n in ast.parse((SOURCE/'tests/test_languages.py').read_text()).body if isinstance(n,ast.FunctionDef) and (n.name.startswith('test_systemverilog_') or n.name.startswith('test_verilog_'))]
    dispatch={k:extract for k,v in facade._DISPATCH.items() if v is original}
    with patch.object(reference,'extract_verilog',extract),patch.object(facade,'extract_verilog',extract),patch.dict(facade._DISPATCH,dispatch):tests=upstream_tests(*selected)
    rng=random.Random(2026092322)
    sources=['','module top; leaf u(); external other(); endmodule\nmodule leaf; endmodule',
       'module top; import pkg::*; function int do_work(input int x); return x; endfunction task work(); endtask endmodule',
       'interface class I; pure virtual function Payload get(); endclass\nclass C extends Base implements I, Other #(Payload);\nrand Box #(Payload) value;\nfunction Item get(input Config x, const ref Box #(Item) y); return value; endfunction\nendclass',
       'class Box #(type T = Payload);\nT item;\nConfig config;\nfunction T get(T value); return value; endfunction\nendclass',
       '/* many\nremoved\nlines */ class A;\nConfig x;\nendclass\nclass B;\nOther y;\nendclass',
       '\u0301class A;\nCafé x;\nendclass\n//字字\nclass B; function Café foo(input Thing x); endfunction endclass',
       'class Invalid; function Config broken;\nclass Next;\nThing t;\nendclass\nendclass']
    for _ in range(400):
        pieces=['module top; leaf u_leaf(); Unknown u_other(); function int f(input int x); f=x; endfunction endmodule','module leaf; endmodule']
        for i in range(rng.randrange(1,5)):
            name=rng.choice(['Item','Box','C','Other']);header=rng.choice(['',' #(type T = Payload)',' extends Base',' implements I, J #(Payload)',' extends Base implements I'])
            body=rng.choices(['Config config;','rand Box #(Payload) value;','local protected Thing x;','T item;','static int count;',
              'function Payload get(input Box #(Thing) x, Config y); return y; endfunction','function Box #(Thing) create(const ref Other o); endfunction',
              'function void skip();\nThing local;\nendfunction','task run(); endtask','// comment\n','/* first\nsecond */','int n;'],k=rng.randrange(12))
            pieces.append('class '+name+header+';\n'+'\n'.join(body)+'\nendclass')
        rng.shuffle(pieces);source='\n'.join(pieces)
        if rng.random()<.2:source=source[:rng.randrange(len(source))]
        sources.append(source)
    with tempfile.TemporaryDirectory(prefix='graphify-verilog-') as tmp:
        root=Path(tmp);path=root/'sample.sv'
        for source in sources:path.write_text(source,encoding='utf-8');extract(path)
        for raw in [b'\xffmodule m; endmodule',b'\xef\xbb\xbfclass A;\nThing x;\nendclass',b'class A;\x00Thing x; endclass']:
            path.write_bytes(raw);extract(path)
        for name in ['two words.v','Café.sv','dir\\file.sv']:
            path=root/name;path.write_text(sources[2]);extract(path)
        extract(root/'absent.v');extract(root)
    types=['','Config','Box #(Payload)','Pair #(Box #(Thing),Other)','T','return','logic','type T = Payload','\x1cThing\x1f','A #(x,,B #(), C)','ÄType','Nameé #(Thing)','X #((B),C)','X #(F #(G #(H)))']
    for _ in range(800):types.append(''.join(rng.choices(list('AB_ xyz(),#')+['字','é','\x1c','\n','\u0301'],k=rng.randrange(70))))
    for value in types:
        expected=reference._sv_split_type_list(value);actual=native.result('verilog.split_types',text=value);helpers+=1
        if expected!=actual:native.fail(text=value,expected=expected,actual=actual)
        for generic,skip in [(False,[]),(True,['T','Config','Thing'])]:
            expected=[list(v) for v in reference._sv_collect_type_refs(value,generic,frozenset(skip))];actual=native.result('verilog.type_refs',text=value,generic=generic,skip=skip);helpers+=1
            if expected!=actual:native.fail(text=value,generic=generic,skip=skip,expected=expected,actual=actual)
    for value in sources[:70]+['a/*//*/b//x\nnext','a//comment\rnext','a/* unclosed','"// not protected"','a/* \u2028 */z']:
        expected=reference._sv_strip_comments(value);actual=native.result('verilog.strip_comments',text=value);helpers+=1
        if expected!=actual:native.fail(text=value,expected=expected,actual=actual)
finally:native.close()
grammar=Language(tree_sitter_verilog.language())
native.receipt('graphify/extractors/verilog.py',['src/extractors/verilog.rs'],extraction_cases=cases,helper_comparisons=helpers,upstream_tests=tests,
    grammar={'python_package':importlib.metadata.version('tree-sitter-verilog'),'rust_crate':'=1.0.3','node_kinds':grammar.node_kind_count,'parse_states':grammar.parse_state_count,'abi':grammar.abi_version},
    comparisons='Exact modules, forward instantiation resolution, external stubs, package imports, functions/tasks, regex class semantics, generic type references and skipped parameters, class boundaries, comment removal, attribution, invalid bytes and errors.',
    adaptations=['Pinned grammar is a compiled native dependency.','Native regex resource-limit errors propagate instead of accepting partial extraction.'])
