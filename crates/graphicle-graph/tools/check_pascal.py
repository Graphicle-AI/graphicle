#!/usr/bin/env python3
"""Differential Pascal port qualification; original tests stay in Python."""
import ast,functools,importlib.util,json,os,random,tempfile
from pathlib import Path
from contextlib import ExitStack
from unittest.mock import patch
from parity import Native,SOURCE,upstream_tests
import graphify.extractors.pascal as reference
import graphify.extractors.resolution as paths
import graphify.extract as exported
spec=importlib.util.spec_from_file_location('_pascal_oracle',SOURCE/'graphify/extractors/pascal.py');oracle=importlib.util.module_from_spec(spec);spec.loader.exec_module(oracle)
native=Native('pascal');counts={};extractions=0
def check(op,expected,**request):
    actual=native.result(op,**request);counts[op]=counts.get(op,0)+1
    if actual!=expected:native.fail(operation=op,request=request,expected=expected,actual=actual)
    return actual
def extract(path,regex=False):
    global extractions
    extractions+=1
    expected=(oracle._extract_pascal_regex if regex else oracle.extract_pascal)(path)
    return check('pascal.regex' if regex else 'pascal.extract',expected,path=str(path))
def clear():
    paths._pascal_unit_cache.clear();paths._pascal_class_stem_cache.clear();native.result('pascal.clear')
try:
    with ExitStack() as stack:
        for module in [reference,exported]:
            stack.enter_context(patch.object(module,'extract_pascal',extract))
            stack.enter_context(patch.object(module,'_extract_pascal_regex',lambda p:extract(p,True)))
        # The form and dispatch-only tests belong to separate module receipts.
        tests=ast.parse((SOURCE/'tests/test_pascal.py').read_text())
        names=[n.name for n in tests.body if isinstance(n,ast.FunctionDef) and n.name.startswith('test_pascal_') and not n.name.endswith(('dispatch_registered','extensions_registered'))]
        tests=upstream_tests(*['tests/test_pascal.py::'+n for n in names],'tests/test_pascal_call_scoping.py')
    rng=random.Random(2026092352)
    texts=['',"{ hi é\n }unit A;","'// retained' (* a *) // End",'interface a implementation b initialization c','implementation\ninterface','A<B, C>, IFoo, TFoo<T<X>>, _Name','A<B\nC>, Foo',"A in 'x.pas',B,C.D,0x;",'begin begin try end end end','begin end; begin']
    for _ in range(300):texts.append(''.join(rng.choices(['Foo','uses ','interface ','implementation ','begin ','end; ','(*','*)','{','}',"'",'//','\n','é','中',',','<','>','in '],k=rng.randrange(30))))
    for text in texts:
        check('pascal.strip',oracle._pascal_strip_comments(text),text=text)
        check('pascal.sections',list(oracle._pascal_split_sections(text)),text=text)
        check('pascal.uses',oracle._pascal_split_uses(text),text=text)
        check('pascal.bases',oracle._pascal_split_bases(text),text=text)
        for start in [0,len(text)//2,len(text),len(text)+2]:check('pascal.body',list(oracle._pascal_find_body(text,start)),text=text,start=start)
    for _ in range(200):
        owners=['module','A','B','C'];records=[('p'+str(i),0,'',rng.choice(owners),rng.choice(['a','b','c'])) for i in range(rng.randrange(15))]
        edges=[{'source':rng.choice(owners),'target':rng.choice(owners),'relation':rng.choice(['inherits','contains'])} for i in range(rng.randrange(10))]
        resolve=oracle._resolve_pascal_callee_factory(records,edges,'module')
        for caller in ['missing']+[r[0] for r in records]:
            for name in ['a','b','c','unknown']:check('pascal.resolve',resolve(caller,name),records=records,edges=edges,module='module',caller=caller,name=name)
    with tempfile.TemporaryDirectory(prefix='graphify-pascal-') as tmp:
        root=Path(tmp);path=root/'Demo.pas';clear()
        for regex in [False,True]:extract(root/'missing.pas',regex);extract(root,regex)
        for i in range(220):
            name='U'+str(i);sep=rng.choice(['\n','\r\n','\r']);methods=['Run','Reset','Prep'];rng.shuffle(methods)
            text=f'''unit {name};
interface
uses SysUtils, Classes;
type
 TBase = class
 procedure Reset;
 end;
 TChild = class(TBase)
 procedure Run;
 procedure Prep;
 end;
implementation
procedure TBase.Reset; begin end;
procedure TChild.Run;
begin
 {rng.choice(['Reset;', 'Self.Reset();','if True then Reset;','begin Reset; end;','Missing(1);'])}
 Prep;
end;
procedure TChild.Prep; begin Run; end;
procedure Helper; begin TChild.Run; end;
end.
'''
            if i%7==0:text=text.replace('TBase','IBase')
            if i%11==0:text=text.replace('unit '+name+';\ninterface','program '+name+';').replace('implementation','')
            if i%13==0:text=text.replace('Reset;',"Reset; { unicode é中 } // comment")
            path.write_bytes(text.replace('\n',sep).encode());extract(path);extract(path,True)
        for data in [b'',b'\xff\x80',b'unit Broken;',b'(* unfinished',b'unit Foo;\ninterface\nimplementation\nend.']:
            path.write_bytes(data);extract(path);extract(path,True)
        (root/'BaseGadget.pas').write_text('unit BaseGadget; interface type TBaseGadget = class procedure Reset; end; implementation procedure TBaseGadget.Reset; begin end; end.')
        (root/'Child.pas').write_text('unit Child; interface uses BaseGadget; type TChild = class(TBaseGadget) procedure Run; end; implementation procedure TChild.Run; begin Reset; end; end.')
        clear();extract(root/'BaseGadget.pas');extract(root/'Child.pas');extract(root/'Child.pas',True)
        for name in ['BaseGadget','basegadget','Missing']:
            check('pascal.resolve_unit',paths._pascal_resolve_unit(path,name),path=str(path),name=name)
        for name in ['TBaseGadget','IBaseGadget','BaseGadget','TMissing']:
            check('pascal.resolve_class',paths._pascal_resolve_class(path,name),path=str(path),name=name)
    grammar=json.loads((Path(__file__).resolve().parents[1]/'vendor/tree-sitter-pascal/SOURCE.json').read_text())
    native.receipt('graphify/extractors/pascal.py',['src/extractors/pascal.rs','src/extractors/pascal_paths.rs'],upstream_tests=tests,operations=counts,extractions=extractions,grammar=grammar,
        adaptations=['The native parser uses the MIT-licensed predecessor pinned in vendor/tree-sitter-pascal/SOURCE.json; AGPL-only 0.11.0 is used only by the Python development oracle.', 'Comparison coverage does not establish support for grammar extensions unique to the AGPL fork. The Graphify traversal and regex fallback are ported; native AST behavior outside the compared syntax can differ.'])
finally:native.close()
