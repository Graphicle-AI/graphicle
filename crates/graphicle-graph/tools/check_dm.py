#!/usr/bin/env python3
"""Compare every DreamMaker-family extractor and its binary/text helpers."""
import ast
import functools
import importlib.metadata
import json
import os
import random
import struct
import tempfile
import zlib
from contextlib import ExitStack
from pathlib import Path
from unittest.mock import patch
from parity import Native, SOURCE, upstream_tests
import graphify.extract as facade
import graphify.extractors.dm as reference
import tree_sitter_dm
from tree_sitter import Language

native=Native('dm');counts={name:0 for name in ('dm','dmi','dmm','dmf')};helpers=0
originals={name:getattr(reference,'extract_'+name) for name in counts}
def wrapper(name):
    original=originals[name]
    @functools.wraps(original)
    def call(path):
        native.use_working_directory(os.getcwd());counts[name]+=1
        try:expected=original(path)
        except Exception as exc:
            actual=native.call('extract.'+name,path=str(path))
            if actual.get('ok') or actual.get('error')!=str(exc):native.fail(kind=name,path=str(path),expected_error=str(exc),actual=actual)
            return None
        actual=native.result('extract.'+name,path=str(path))
        if expected!=actual:native.fail(kind=name,path=str(path),source=path.read_bytes().hex() if path.is_file() and path.stat().st_size<1000000 else None,expected=expected,actual=actual)
        return actual
    return call
wrapped={name:wrapper(name) for name in counts}
def helper(name,fn,**request):
    global helpers
    helpers+=1
    try:expected=fn()
    except Exception as exc:
        actual=native.call('dm.'+name,**request)
        if actual.get('ok') or actual.get('error')!=str(exc):native.fail(helper=name,request=request,expected_error=str(exc),actual=actual)
        return
    actual=native.result('dm.'+name,**request)
    if expected!=actual:native.fail(helper=name,request=request,expected=expected,actual=actual)
def chunk(kind,data,length=None):return struct.pack('>I',len(data) if length is None else length)+kind+data+bytes(4)
def png(body,compressed=True,keyword=b'Description'):
    return b'\x89PNG\r\n\x1a\n'+chunk(b'zTXt' if compressed else b'tEXt',keyword+b'\0'+(b'\0'+zlib.compress(body) if compressed else body))
try:
    selected=['tests/test_languages.py::'+n.name for n in ast.parse((SOURCE/'tests/test_languages.py').read_text()).body if isinstance(n,ast.FunctionDef) and any(n.name.startswith('test_'+k+'_') for k in counts)]
    with ExitStack() as stack:
        for name,fn in wrapped.items():
            stack.enter_context(patch.object(reference,'extract_'+name,fn));stack.enter_context(patch.object(facade,'extract_'+name,fn))
        stack.enter_context(patch.dict(facade._DISPATCH,{k:wrapped[name] for k,v in facade._DISPATCH.items() for name,original in originals.items() if v is original}))
        tests=upstream_tests(*selected)
    rng=random.Random(2026092328)
    dm=['','#include "exists.dm"\n#include "missing.dm"\n',
        '/datum/item\n\tproc/work()\n\t\thelper()\n/proc/helper()\n\treturn 1\n',
        '/datum/item/proc/work()\n\tnew /datum/item()\n\t..()\n\tobj.work()\n',
        '/datum/item\n\tNew()\n\t\thelp()\n/proc/help()\n\treturn\n',
        '/proc/a()\n\tb()\n/proc/b()\n\ta()\n/proc/a()\n\ta()\n']
    for _ in range(350):
        lines=[]
        for _ in range(rng.randrange(7)):
            typ=rng.choice(['/datum/a','/datum/b','/mob/a','/obj/item','/area']);name=rng.choice(['work','New','help','attack','test'])
            header=rng.choice(['/proc/'+name+'()',typ+'/proc/'+name+'()',typ+'/'+name+'()',typ+'\n\tproc/'+name+'()'])
            body='\n'.join('\t'+('\t' if '\n' in header else '')+rng.choice(['work()','help()','a.work()','new /datum/a()','..()','return 1','obj.attack()']) for _ in range(rng.randrange(6)))
            lines.append(header+'\n'+body)
        text='\n'.join(lines)
        if text and rng.random()<.15:text=text[:rng.randrange(len(text))]
        dm.append(text)
    maps=['','"a" = (/turf/a,/obj/b{v="a,b";x=list(1,2)},/area/c)\n(1,1,1)={"\nxx\n"}',
          '"a" = (/obj/a\n,/obj/b{v="(a,b)"})\n"b" = (/obj/c)\n(١,٢,٣) = "x"\n"z"=(/obj/ignored)']
    forms=['','elem "orphan"\ntype = MAP\n','window "a"\nelem "b"\ntype = MAP\ntype = OTHER\nwindow "a"\nelem "b"\ntype = NEW',
           'window "a ["\nelem "b ["\ntype = MAP\n','window "é"\nelem "b"\ntype= MAP\x1f\n']
    for _ in range(250):
        entries=rng.choices(['/obj/a','/turf/b','/area/c','/obj/a{v="a,b"}','/obj/x{a=list(1,2)}','bad',''],k=rng.randrange(8))
        maps.append('"a" = ('+',\n'.join(entries)+')\n'+rng.choice(['','(1,1,1) = "a"\n"x" = (/ignored)','broken(']))
        forms.append('\n'.join(rng.choices(['window "a"','window "b"','elem "x"','elem "y"','type = MAP','type = MAIN','type = OUTPUT','window "a" garbage','elem ""','\twindow "Café"\u2000','\x1ftype = TYPE\x1f'],k=rng.randrange(40))))
    descriptions=[b'',b'# BEGIN DMI\nstate = "mob"\nstate = ""\nstate = walk\nstate = "mob"\n',b'\xffstate = "a"\r\nstate = "b"',
                  ('state = "é"\x1cstate = "β"\u2028state = "é"').encode()]
    for _ in range(160):descriptions.append('\n'.join('state = '+rng.choice(['"a"','"b"','"Café"','""','"(odd)"','plain','"']) for _ in range(rng.randrange(20))).encode())
    binary=[b'',b'not png',b'\x89PNG\r\n\x1a\n',png(b'state = "x"',keyword=b'Other'),b'\x89PNG\r\n\x1a\n'+chunk(b'zTXt',b'missing-nul')]
    binary += [png(body,compressed) for body in descriptions for compressed in [False,True]]
    binary += [png(b'x'*(1024*1024+40))]
    packed=png(b'state = "partial"\n'*300)
    binary += [packed[:i] for i in range(8,len(packed))]
    binary += [b'\x89PNG\r\n\x1a\n'+chunk(b'zTXt',b'Description\0\0'+bad) for bad in [b'invalid',b'',b'\0',b'\x78\x9c',zlib.compress(b'x')[:-2],zlib.compress(b'x')[:-1]+b'\0']]
    with tempfile.TemporaryDirectory(prefix='graphify-dm-') as tmp:
        root=Path(tmp);(root/'exists.dm').write_text('/datum/exists\n')
        for kind,texts in [('dm',dm),('dmm',maps),('dmf',forms)]:
            path=root/('sample.'+kind)
            for text in texts:path.write_text(text);wrapped[kind](path)
            for raw in [b'\xff invalid\n',b'\xef\xbb\xbf// bom\r\n',b'\0\0']:
                path.write_bytes(raw);wrapped[kind](path)
            wrapped[kind](root/('missing.'+kind));wrapped[kind](root)
        path=root/'sample.dmi'
        for data in binary:
            path.write_bytes(data);wrapped['dmi'](path);helper('description',lambda:reference._read_dmi_description(data),data=list(data))
        wrapped['dmi'](root/'missing.dmi');wrapped['dmi'](root)
        path=root/'large.dmm'
        with path.open('wb') as f:f.truncate(50*1024*1024+1)
        wrapped['dmm'](path)
        path=root/'sample.dm';path.write_text(dm[1]);saved=os.getcwd()
        try:os.chdir(root);wrapped['dm'](Path('sample.dm'))
        finally:os.chdir(saved)
    for _ in range(1200):
        text=''.join(rng.choices(list('/(){}[],\\" abc\n\t')+['é','\x1f','\u2003'],k=rng.randrange(100)))
        helper('split_tile',lambda:reference._split_dmm_tile(text),text=text)
        helper('type_path',lambda:reference._dmm_type_path(text),text=text)
finally:native.close()
grammar=Language(tree_sitter_dm.language())
native.receipt('graphify/extractors/dm.py',['src/extractors/dm.rs'],extraction_cases=counts,helper_comparisons=helpers,upstream_tests=tests,
    grammar={'python_package':importlib.metadata.version('tree-sitter-dm'),'rust_crate':'=0.25.1','node_kinds':grammar.node_kind_count,'parse_states':grammar.parse_state_count,'abi':grammar.abi_version,'sources':'dm-grammar.json confirms identical parser and scanner sources'},
    comparisons='Exact source graph/calls/imports, binary PNG description parsing, 1 MiB bounded decompression, compressed/truncated/corrupt metadata, state IDs, quoted/nested map dictionary, grid exclusion, 50 MiB map limit, form ownership/type labels, malformed input and read failures.',
    adaptations=['Uses the native zlib backend for the same bounded decompression/error contract. Does not add PNG image decoding or CRC validation absent from the source.'])
