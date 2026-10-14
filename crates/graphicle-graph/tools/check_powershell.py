#!/usr/bin/env python3
"""PowerShell script and manifest parity, including original dispatch fixtures."""
import ast
import importlib.metadata
import random
import tempfile
from pathlib import Path
from unittest.mock import patch
from parity import Native,SOURCE,upstream_tests
import graphify.extract as facade
import graphify.extractors.powershell as reference
import tree_sitter_powershell
from tree_sitter import Language

native=Native('powershell');original=reference.extract_powershell;manifest_original=reference.extract_powershell_manifest;cases=0;manifests=0;helpers=0
def extract(path):
    global cases
    expected=original(path);actual=native.result('extract.powershell',path=str(path));cases+=1
    if expected!=actual:native.fail(path=str(path),source=path.read_bytes().hex() if path.is_file() else None,expected=expected,actual=actual)
    return actual
def manifest(path):
    global manifests
    expected=manifest_original(path);actual=native.result('extract.powershell_manifest',path=str(path));manifests+=1
    if expected!=actual:native.fail(path=str(path),source=path.read_bytes().hex() if path.is_file() else None,expected=expected,actual=actual)
    return actual
try:
    selected=['tests/test_languages.py::'+n.name for n in ast.parse((SOURCE/'tests/test_languages.py').read_text()).body if isinstance(n,ast.FunctionDef) and n.name.startswith('test_powershell_')]
    dispatch={k:extract if v is original else manifest for k,v in facade._DISPATCH.items() if v is original or v is manifest_original}
    with patch.object(reference,'extract_powershell',extract),patch.object(reference,'extract_powershell_manifest',manifest),patch.object(facade,'extract_powershell',extract),patch.object(facade,'extract_powershell_manifest',manifest),patch.dict(facade._DISPATCH,dispatch):tests=upstream_tests(*selected)
    rng=random.Random(2026092323)
    sources=['','function Foo { Bar; Foo; Unknown; Bar }\nfunction Bar { foo; Import-Module Other; . ./Shared.psm1 }',
       'enum Color { Red=1; Blue=2 }\nclass Item : Base, IFoo { [Color] $color; [Other] Make([Item] $x) { Foo; return $x } }',
       'class Base {}\nclass Child : Base, IFace, Other { [Thing] $field; Child() { External }\n [Thing] Work([Arg] $value) { Work; Import-Module Hidden } }',
       'using namespace System.IO\nusing module Other\nImport-Module -Force First -Name Second.psm1\n. .\\Shared.psm1',
       'function Outer { function Inner { Deep }; Inner; Outer; . ./Nested.psm1 }',
       'function Build-X { & Other; . Other; return 1; Write-Host "hello" }',
       'class Café { [Thing] $x; [Thing] Faire([Other] $y) { External; } }']
    for _ in range(400):
        pieces=rng.choices(['enum Color { Red; Blue; Green }','class Thing {}','Import-Module -Name Some.Module','using namespace System.IO','. ../Shared.psm1'],k=rng.randrange(4))
        for _ in range(rng.randrange(1,7)):
            name=rng.choice(['Foo','Bar','Work-X','Nested']);body='; '.join(rng.choices(['Foo','Bar','Unknown','Import-Module Util','. ./Shared.psm1','return 1','Write-Host x','function Inner { Foo }; Inner','$x=Bar'],k=rng.randrange(8)))
            pieces.append(f'function {name} {{ {body} }}')
        pieces.append(rng.choice(['class Child : Thing, IFoo { [Color] $color; [Thing] Get([Arg] $x) { Foo; Bar; return $x } }','class Thing : Base { [Other] $field; Thing() { External } }','']))
        text='\n'.join(pieces)
        if rng.random()<.2:text=text[:rng.randrange(len(text))]
        sources.append(text)
    with tempfile.TemporaryDirectory(prefix='graphify-powershell-') as tmp:
        root=Path(tmp);path=root/'sample.ps1'
        for text in sources:path.write_text(text,encoding='utf-8');extract(path)
        contents=["@{RootModule='Main.psm1'; NestedModules=@('A.psm1','./dir/B.psm1'); RequiredModules=@('Direct',@{ModuleName='Pester'; ModuleVersion='5.0'})}",
          "@{'RootModule'='quoted'; rootmodule='wrongcase'; PrivateData=@{RequiredModules='not-top'}; RequiredModules=@{ModuleName=@('A','B'); Other=@{ModuleName='nested'}}}",
          "@{RequiredModules=@(@{ModuleName='A'}, 'Direct', @{ModuleVersion='9.0'; Guid='abc'}, @{ModuleName='B'})}",
          "@{NestedModules=@{Version='1.2'; Module='Something'}; RootModule=''}",'','@{RequiredModules=']
        for _ in range(250):
            values=["'Direct'","@{ModuleName='Pester';ModuleVersion='5.0'}","'./path/Inner.psm1'","@{Guid='abc'; ModuleName='Other'}","'../Shared'",'"$PSScriptRoot/Module.psm1"',"@{Other='skip'; ModuleName=@('A','B')}"]
            text="@{\nRootModule='Main.psm1'\nNestedModules=@('Extra','Extra')\nRequiredModules=@("+','.join(rng.choices(values,k=rng.randrange(10)))+")\nPrivateData=@{RequiredModules='skip'}\n}"
            if rng.random()<.1:text=text[:rng.randrange(len(text))]
            contents.append(text)
        path=root/'sample.psd1'
        for text in contents:path.write_text(text,encoding='utf-8');manifest(path)
        for raw in [b'\xfffunction F { G }',b'\xef\xbb\xbf@{RootModule="A"}',b'\x00@{RequiredModules=@("A")}']:
            path.write_bytes(raw);extract(path);manifest(path)
        for file in [root/'absent.ps1',root]:extract(file);manifest(file)
    names=['./sub/Util.psm1','\\sub\\A.psm1','Name','..','.hidden','file.','A.B.C','C:\\dir\\File.psm1','"Name"','\x1c Name \x1f','dir/A\n.psm1']
    for _ in range(1000):names.append(''.join(rng.choices(list('abcXYZ./\\_ 09')+['\n','é','字','\x1c'],k=rng.randrange(40))))
    for name in names:
        expected=reference._psd1_module_name(name);actual=native.result('powershell.module_name',raw=name);helpers+=1
        if expected!=actual:native.fail(raw=name,expected=expected,actual=actual)
finally:native.close()
grammar=Language(tree_sitter_powershell.language())
native.receipt('graphify/extractors/powershell.py',['src/extractors/powershell.rs'],script_cases=cases,manifest_cases=manifests,helper_comparisons=helpers,upstream_tests=tests,
    grammar={'python_package':importlib.metadata.version('tree-sitter-powershell'),'rust_crate':'=0.26.4','node_kinds':grammar.node_kind_count,'parse_states':grammar.parse_state_count,'abi':grammar.abi_version},
    comparisons='Exact function/class/enum/property/method nodes, inheritance, imports including function-body and dot sourcing, call deduplication and raw calls, type stubs, PSD1 arrays/module-specification hashes, key sensitivity, nested exclusions, bytes and errors.',
    adaptations=['Pinned grammar compiled into the native binary. Original dispatch fixtures retain Python routing with both PowerShell handlers replaced by Rust; the full extraction dispatcher is tracked separately.'])
