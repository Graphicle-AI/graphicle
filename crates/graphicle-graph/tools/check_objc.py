#!/usr/bin/env python3
"""Objective-C parser, category, field-table and include-resolution parity."""
import ast
import functools
import importlib.metadata
import itertools
import json
import os
import random
import tempfile
from pathlib import Path
from unittest.mock import patch
from parity import Native,SOURCE,upstream_tests
import graphify.extract as facade
import graphify.extractors.objc as reference
import graphify.extractors.resolution as resolution
import tree_sitter_objc
from tree_sitter import Language

native=Native('objc');original=reference.extract_objc;cases=0;helpers=0
def ordered(data):
    # Python's process-randomized set controls the target order of contiguous
    # message-call fanout. Preserve node order, all non-call edge order and each
    # call's position/source/line/attributes, sorting only those target runs.
    result=dict(data);edges=[]
    def group(edge):
        if edge.get('relation')!='calls':return None
        return json.dumps({k:v for k,v in edge.items() if k!='target'},sort_keys=True)
    for key,run in itertools.groupby(data.get('edges',[]),key=group):
        run=list(run);edges.extend(sorted(run,key=lambda e:json.dumps(e,sort_keys=True)) if key is not None else run)
    result['edges']=edges
    return result
@functools.wraps(original)
def extract(path):
    global cases
    native.use_working_directory(os.getcwd())
    expected=original(path);actual=native.result('extract.objc',path=str(path));cases+=1
    if ordered(expected)!=ordered(actual):native.fail(path=str(path),source=path.read_bytes().hex() if path.is_file() else None,expected=expected,actual=actual)
    return actual
def include(raw,path):
    global helpers
    native.use_working_directory(os.getcwd());expected=resolution._resolve_c_include_path(raw,path)
    expected=str(expected) if expected is not None else None
    actual=native.result('resolution.c_include',raw=raw,source=path);helpers+=1
    if expected!=actual:native.fail(raw=raw,path=path,expected=expected,actual=actual)
    return actual
try:
    selected=['tests/test_languages.py::'+n.name for n in ast.parse((SOURCE/'tests/test_languages.py').read_text()).body if isinstance(n,ast.FunctionDef) and n.name.startswith('test_objc_')]
    selected+=['tests/test_objc_category_interfaces.py','tests/test_objc_property_ivar_receivers.py','tests/test_objc_member_calls.py','tests/test_objc_enum_declarations.py']
    selected+=['tests/test_cpp_objc_cross_file_calls.py::'+n.name for n in ast.parse((SOURCE/'tests/test_cpp_objc_cross_file_calls.py').read_text()).body if isinstance(n,ast.FunctionDef) and n.name.startswith('test_objc_')]
    dispatch={k:extract for k,v in facade._DISPATCH.items() if v is original}
    with patch.object(reference,'extract_objc',extract),patch.object(facade,'extract_objc',extract),patch.dict(facade._DISPATCH,dispatch):tests=upstream_tests(*selected)
    rng=random.Random(2026092326)
    sources=['','@interface Foo : NSObject <Proto,Other>\n@property(nonatomic) NSArray<Item *> *items;\n@property Thing *field;\n- (void)go;\n@end',
      '@interface Foo (Extra)\n- (void)more;\n@end\n@implementation Foo (Extra)\n- (void)more { [self other]; }\n@end',
      '@protocol P <Base, Other>\n-(void)go;\n@end\n@implementation Foo\n-(void)go { [[Item alloc] init]; @selector(other:with:); }\n-(void)other:(id)x with:(id)y {}\n@end',
      '@interface Foo : NSObject { Thing *_field; Other *a,*b; }\n@property Thing *field;\n@property Other *field;\n@end\n@implementation Foo\n-(void)go { Thing *x; Other *x; [x act]; [self.field act]; [_field act]; [Foo.shared act]; }\n@end',
      '@implementation A\n-(void)name {}\n-(void)surname {}\n-(void)go { self.name; @selector(name); [self name]; }\n@end\n@implementation B\n-(void)name {}\n@end',
      '#import "Other.h"\n#import <Foundation/Foundation.h>\n@import Foundation.NSString;\nNS_ASSUME_NONNULL_BEGIN\n@interface A : NSObject\n@end\nNS_ASSUME_NONNULL_END']
    sources+=['typedef NS_ENUM(NSInteger, Mode) { ModeA, ModeB = 3 };\nenum Color { Red, Green };\ntypedef enum { Left, Right } Side;\ntypedef enum Shade { Dark } Shade;\nenum Bodyless;\nenum { Loose };','@implementation Foo\n-(Mode)mode { return ModeA; }\n@end\nenum Flags : unsigned int { FlagOne, FlagTwo };']
    for _ in range(260):
        pieces=[]
        for i in range(rng.randrange(1,4)):
            name=rng.choice(['Foo','Bar','Item','Worker']);category=rng.choice(['',' (Extra)',' ()']);fields='\n'.join(rng.choices(['@property Thing *field;','@property Other *field;','@property NSArray<Item *> *items;','@property id<Proto> proto;'],k=rng.randrange(5)))
            pieces.append(f'@interface {name}{category} : NSObject <Proto>\n'+fields+'\n-(void)work;\n@end')
            methods=[]
            for j in range(rng.randrange(1,5)):
                method=rng.choice(['work','name','other','surname']);body=' '.join(rng.choices(['[self work];','[Foo name];','[self.field other];','[_field other];','self.name;','@selector(name);','[[Item alloc] init];','Thing *local; [local other];','Thing *a,*b;','[Foo.shared work];'],k=rng.randrange(6)))
                methods.append(f'-(void){method} {{ {body} }}')
            pieces.append(f'@implementation {name}{category}\n'+'\n'.join(methods)+'\n@end')
        text='\n'.join(pieces)
        if rng.random()<.2:text=text[:rng.randrange(len(text))]
        sources.append(text)
    with tempfile.TemporaryDirectory(prefix='graphify-objc-') as tmp:
        root=Path(tmp);(root/'Other.h').write_text('@interface Other\n@end');path=root/'Foo+Extra.m'
        for text in sources:path.write_text(text,encoding='utf-8');extract(path)
        for raw in [b'\xff@implementation A\n-(void)go{}\n@end',b'\xef\xbb\xbf@interface A\n@end',b'@interface A\x00\n@end']:
            path.write_bytes(raw);extract(path)
        extract(root/'missing.m');extract(root)
        for name in ['Foo.h','Foo+Extra.h','C++Bridge.h','Foo+.h','dir+extra/Foo+More.m']:
            path=root/name;path.parent.mkdir(parents=True,exist_ok=True);path.write_text(sources[2]);extract(path)
        for raw in ['Other.h','./Other.h','missing.h','','./','../unknown','dir+extra/../Other.h',str(root/'Other.h')]:include(raw,str(root/'main.m'))
        (root/'Second.h').write_text('@interface Second\n@end');link=root/'Link.h';link.symlink_to(root/'Other.h')
        resolution._cached_realpath.cache_clear();native.result('resolution.clear_paths');first=include('Link.h',str(root/'main.m'))
        link.unlink();link.symlink_to(root/'Second.h');assert include('Link.h',str(root/'main.m'))==first
        resolution._cached_realpath.cache_clear();native.result('resolution.clear_paths');assert include('Link.h',str(root/'main.m'))==str((root/'Second.h').resolve())
        saved=os.getcwd()
        try:
            os.chdir(root);extract(Path('Foo+Extra.m'));include('Other.h','main.m')
        finally:os.chdir(saved)
    stems=['Foo+Cat','dir/Foo+Cat','C++Bridge','Foo+','+Cat','Foo++Cat','Foo+Bar+Baz','/Foo+Cat','a/../Foo+Extra','Café+More','Foo+_Cat']
    for _ in range(800):stems.append(''.join(rng.choices(list('ABCxyz_09/+')+['é','\n','\x1c'],k=rng.randrange(30))))
    for stem in stems:
        expected=reference._objc_category_base_stem(stem);actual=native.result('objc.category_stem',stem=stem);helpers+=1
        if expected!=actual:native.fail(stem=stem,expected=expected,actual=actual)
finally:native.close()
grammar=Language(tree_sitter_objc.language())
native.receipt('graphify/extractors/objc.py',['src/extractors/objc.rs','src/extractors/engine_declarator.rs','src/extractors/resolution_paths.rs'],extraction_cases=cases,helper_comparisons=helpers,upstream_tests=tests,
    grammar={'python_package':importlib.metadata.version('tree-sitter-objc'),'rust_crate':'=3.0.2','node_kinds':grammar.node_kind_count,'parse_states':grammar.parse_state_count,'abi':grammar.abi_version},
    comparisons='Exact categories/protocols/methods, macro masking, typed properties/ivars and tombstones, local receiver typing, allocations, compound selectors, dot access and raw calls, include cache lifetime, malformed bytes and failures.',
    adaptations=['Message-call fanout iterates a Python randomized set. Rust uses sorted IDs; only contiguous same-source/same-location call-target runs are canonicalized for comparison. Node order and non-call edge order remain exact.','Original multi-file fixtures retain Python corpus resolution; C++ declarator and cached include helpers are native dependencies, not a claim that engine.py/resolution.py are complete.'])
