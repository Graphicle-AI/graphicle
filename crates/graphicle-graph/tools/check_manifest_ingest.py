#!/usr/bin/env python3
"""Native package manifests versus their pinned parsers and original integration tests."""
import functools
import importlib.util
import json
import os
import random
import tempfile
from contextlib import ExitStack
from pathlib import Path
from unittest.mock import patch
from parity import Native,SOURCE,upstream_tests
import graphify.manifest_ingest as reference
import graphify.extract as facade

spec=importlib.util.spec_from_file_location('_graphify_manifest_oracle',SOURCE/'graphify/manifest_ingest.py')
oracle=importlib.util.module_from_spec(spec);spec.loader.exec_module(oracle)
native=Native('manifest_ingest');cases=helpers=0;diagnostics=[]
def compare(op,expected,**data):
    actual=native.result(op,**data)
    if actual!=expected:native.fail(operation=op,request=data,expected=expected,actual=actual)
    return actual
@functools.wraps(reference.extract_package_manifest)
def extract(path):
    global cases
    native.use_working_directory(os.getcwd())
    expected=oracle.extract_package_manifest(path);actual=native.result('manifest.extract',path=str(path));cases+=1
    if actual!=expected:
        if expected.get('error','').startswith('manifest parse error:') and actual.get('error','').startswith('manifest parse error:'):
            assert expected['nodes']==actual['nodes']==[] and expected['edges']==actual['edges']==[]
            diagnostics.append({'expected':expected['error'],'actual':actual['error']})
        else:native.fail(path=str(path),text=path.read_text(errors='replace') if path.is_file() else None,expected=expected,actual=actual)
    return actual
def is_path(path):
    global helpers
    helpers+=1;return compare('manifest.is_path',oracle.is_package_manifest_path(path),path=str(path))
def parse(eco,text):
    global helpers
    helpers+=1
    fn={'apm':oracle._parse_apm,'apm_fallback':oracle._parse_apm_fallback,'python':oracle._parse_pyproject,
        'cargo':oracle._parse_cargo,'go':oracle._parse_gomod,'maven':oracle._parse_pom}[eco]
    try:expected=fn(text)
    except Exception as e:
        actual=native.call('manifest.parse',ecosystem=eco,text=text)
        if actual.get('ok'):native.fail(ecosystem=eco,text=text,expected_error=str(e),actual=actual)
        diagnostics.append({'expected':str(e),'actual':actual['error']})
        return None
    return compare('manifest.parse',expected,ecosystem=eco,text=text)
def fallback(text):return parse('apm_fallback',text)
try:
    assert native.result('manifest.names')==oracle.PACKAGE_MANIFEST_NAMES
    with ExitStack() as stack:
        for module in [reference,facade]:
            for name,fn in [('extract_package_manifest',extract),('is_package_manifest_path',is_path)]:
                if hasattr(module,name):stack.enter_context(patch.object(module,name,fn))
        stack.enter_context(patch.object(reference,'_parse_apm_fallback',fallback))
        tests=upstream_tests('tests/test_manifest_ingest.py','tests/test_apm_fallback_version.py')
    forms={
        'apm.yml':('apm',[
            '', 'name: a\nversion: 1.2.3\ndependencies:\n  - b\n  - a\n  - b\n',
            'name: a\ndependencies: {b: ">=1", c: {version: 2}, d: null}',
            'name: a\ndependencies: [b, {c: ">=1"}, {}, 3, false]',
            'name: a\nversion: 1.5\n', 'name: a\nversion: 5\n',
            'name: "on"\ndependencies: ["yes", "no", "off"]',
            'base: &base {name: base, dependencies: [b]}\n<<: *base\nname: child\n',
            'name: a\nname: b\ndependencies: []','name: a\nbroken: [','[a,b]']),
        'pyproject.toml':('python',[
            '', '[project]\nname="a"\nversion="1"\ndependencies=["x>=1", "y[extra]; os_name == \'x\'"]',
            '[tool.poetry]\nname="a"\nversion="2"\n[tool.poetry.dependencies]\npython="^3.12"\nx="*"\ny="*"',
            '[project]\nname="a"\ndependencies=["b","a",3]\n[tool.poetry]\nname="ignored"\n[tool.poetry.dependencies]\nx="1"\nPython="*"',
            '[project]\nname="a"\ndependencies="xy"', '[project]\nname="a"\ndependencies=4',
            '[project]\nname="a"\n[project.dependencies]\nb="1"\nc="2"',
            'project="x"\n[tool.poetry]\nname="from-poetry"','[broken']),
        'Cargo.toml':('cargo',[
            '', '[workspace]\nmembers=["a","b"]', '[package]\nname="a"\nversion.workspace=true',
            '[package]\nname="a"\n[dependencies]\nz="1"\na="1"\nb={package="renamed"}\n[dev-dependencies]\nc="1"\n[target."cfg(unix)".dependencies]\ny="2"\nx="1"',
            '[package]\nname=5\nversion="1"', '[package]\nname="a"\nversion=1',
            'dependencies=["a", {b="1"}]\n[package]\nname="pkg"', '[package]\nname="a"\nbroken=']),
        'go.mod':('go',[
            '', 'module example.com/a\nrequire example.com/z v1\nrequire (\n example.com/b v2\n //skip\n example.com/a v1\n)\n',
            'module first\nmodule second\nrequire x v1\nrequire y no-version\n',
            '\x1fmodule\x1fexample.com/é\nrequire (\nx v0\n)\n',
            'require (\na v1\nmodule later\nb v2\n)', 'module m\nrequire (a v1)\nb v2\n)']),
        'pom.xml':('maven',[
            '<project><artifactId>a</artifactId><version>1</version></project>',
            '<project xmlns="urn:m"><groupId>g</groupId><artifactId>a</artifactId><dependencies><dependency><artifactId>b</artifactId></dependency></dependencies></project>',
            "<project xmlns='urn:m'><artifactId>a</artifactId></project>",
            '<project><artifactId>a<!--comment--><?instruction x?>tail</artifactId><version/><profiles><profile><dependencies><dependency><groupId>g</groupId><artifactId>b</artifactId></dependency></dependencies></profile></profiles></project>',
            '<project><artifactId>a<inner/>b</artifactId><dependencies><dependency><artifactId><inner/>b</artifactId></dependency></dependencies></project>',
            '<!DOCTYPE project [<!ENTITY dep "lib">]><project><artifactId>&dep;</artifactId></project>',
            '<project><artifactId/><groupId>g</groupId></project>', '<project><bad>', ''])}
    rng=random.Random(2026092334)
    with tempfile.TemporaryDirectory(prefix='graphify-manifest-') as tmp:
        root=Path(tmp)
        for filename,(eco,texts) in forms.items():
            path=root/filename
            for text in texts:path.write_text(text);extract(path);parse(eco,text)
        for _ in range(420):
            kind=rng.choice(list(forms));eco=forms[kind][0];name=rng.choice(['core','package','other','éclair','a.b','a_b'])
            deps=rng.choices(['x','z','core','éclair','some/pkg'],k=rng.randrange(7))
            if eco=='apm':text='name: '+name+'\nversion: 1.2.3\ndependencies:\n'+''.join('  - '+d+'\n' for d in deps)
            elif eco=='python':text='[project]\nname='+json.dumps(name,ensure_ascii=False)+'\ndependencies='+json.dumps([d+rng.choice(['','>=1','[extra]']) for d in deps],ensure_ascii=False)
            elif eco=='cargo':text='[package]\nname='+json.dumps(name,ensure_ascii=False)+'\n[dependencies]\n'+''.join(json.dumps(d,ensure_ascii=False)+'="1"\n' for d in dict.fromkeys(deps))
            elif eco=='go':text='module '+name+'\nrequire (\n'+''.join(d+' v1\n' for d in deps)+')\n'
            else:text='<project><artifactId>'+name+'</artifactId><dependencies>'+''.join('<dependency><artifactId>'+d+'</artifactId></dependency>' for d in deps)+'</dependencies></project>'
            path=root/kind;path.write_text(text);extract(path);parse(eco,text)
        path=root/'apm.yml';path.write_text('x'*2_000_001);extract(path)
        for kind in forms:
            extract(root/'absent'/kind)
        path=root/'go.mod'
        path.write_bytes(b'\xffmodule a\r\nmodule b\rrequire x v1\n');extract(path)
        saved=os.getcwd()
        try:os.chdir(root);extract(Path('go.mod'))
        finally:os.chdir(saved)
    for _ in range(750):
        text='\n'.join(rng.choices(['name: a','name: "b"','version: 1.2.3','dependencies:','  - x','  y: "1"','other: z','  version: 9','\tname: wrong'],k=rng.randrange(12)))
        fallback(text)
    for _ in range(750):
        value=rng.choice([None,True,False,0,'abc',[],{},['x',{},None,{'b':4,'a':3}],{'z':1,'b':4}]);helpers+=1
        compare('manifest.coerce_deps',oracle._coerce_deps(value),value=value)
        spec=rng.choice([' requests>=2','pkg[extra] ==2','a; x','éclair','a(b)','', 'a\x1fb']);helpers+=1
        compare('manifest.pep508_name',oracle._pep508_name(spec),spec=spec)
        is_path(Path(rng.choice(['apm.yml','APM.YAML','Cargo.toml','POM.XML','go.mod','thing.toml','project.json'])))
finally:native.close()
native.receipt('graphify/manifest_ingest.py',['src/manifest_ingest.rs'],extraction_cases=cases,helper_cases=helpers,upstream_tests=tests,
    native_parse_diagnostics=diagnostics,
    comparisons='Canonical package IDs, ordered dependency extraction, TOML/Python/Poetry/Cargo target dependencies, Go require blocks, Maven namespaces/entities, APM YAML and line fallback, oversized/read/parse failures and original corpus merge/prune tests.',
    adaptations=['Native parser errors preserve failure categories and empty graphs, with native diagnostic wording.',
        'TOML and YAML parsers are native linked dependencies; Python missing-tomli import mocks do not apply. The original APM fallback remains separately callable.',
        'The original Python corpus/build consumer is retained for integration tests; the manifest parser APIs run in Rust.'])
