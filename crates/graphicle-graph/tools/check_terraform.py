#!/usr/bin/env python3
"""Compare the whole Terraform module, including cached/incremental topology."""
import copy
import dataclasses
import functools
import importlib.metadata
import json
import os
import random
import tempfile
import types
from contextlib import ExitStack
from pathlib import Path
from unittest.mock import patch
from parity import Native, SOURCE, ROOT, upstream_tests
from naming import native_input_path, to_upstream
from upstream_fixes import oracle_source
from upstream_cli import in_process_graphify
import graphify.extract as facade
import graphify.extractors.terraform as reference
import graphify.resolver_registry as registry
import tree_sitter_hcl
from tree_sitter import Language

# Expected values come from the unchanged pinned module. The imported module
# keeps its identity for patching the facade and resolver registry.
oracle=types.ModuleType('reference_terraform');oracle.__package__='graphify.extractors'
exec(compile(oracle_source('graphify/extractors/terraform.py'),'<pinned terraform.py with reviewed fixes>','exec'),oracle.__dict__)
native=Native('terraform');cases=0;helpers=0
original=reference.extract_terraform
original_prepare=oracle.prepare_terraform
original_resolve=reference.resolve_terraform_modules
original_refresh=oracle.refresh_terraform_paths
def compare(op,expected,**args):
    global helpers
    native.use_working_directory(os.getcwd());actual=native.result(op,**args);helpers+=1
    if expected!=actual:native.fail(operation=op,request=args,expected=expected,actual=actual)
    return actual
@functools.wraps(original)
def extract(path):
    global cases
    native.use_working_directory(os.getcwd());expected=oracle.extract_terraform(path);actual=native.result('extract.terraform',path=str(path));cases+=1
    if expected!=actual:native.fail(path=str(path),source=path.read_bytes().hex() if path.is_file() else None,expected=expected,actual=actual)
    return actual
def prepare(result,path,root):
    expected=copy.deepcopy(result);original_prepare(expected,path,root)
    actual=compare('terraform.prepare',expected,data=result,path=str(path),root=str(root))
    result.clear();result.update(actual)
def resolve(per_file,nodes,edges):
    expected_nodes=copy.deepcopy(nodes);expected_edges=copy.deepcopy(edges)
    oracle.resolve_terraform_modules(copy.deepcopy(per_file),expected_nodes,expected_edges)
    actual=compare('terraform.resolve',dict(nodes=expected_nodes,edges=expected_edges),per_file=per_file,nodes=nodes,edges=edges)
    nodes[:]=actual['nodes'];edges[:]=actual['edges']
def refresh(changed,corpus,invalidated=()):
    global helpers
    expected=[str(p) for p in original_refresh(changed,corpus,invalidated)]
    # An ignore-file change refreshes every module; the engine's ignore file
    # is .graphicleignore, so path names are translated both ways.
    names=lambda paths:[str(native_input_path(p)) for p in paths]
    native.use_working_directory(os.getcwd());helpers+=1
    actual=to_upstream(native.result('terraform.refresh',changed=names(changed),corpus=names(corpus),invalidated=names(invalidated)))
    if expected!=actual:native.fail(operation='terraform.refresh',request=dict(changed=names(changed),corpus=names(corpus),invalidated=names(invalidated)),expected=expected,actual=actual)
    return [Path(p) for p in actual]
try:
    with ExitStack() as stack:
        for mod,name,fn in [(reference,'extract_terraform',extract),(facade,'extract_terraform',extract),(reference,'prepare_terraform',prepare),(facade,'prepare_terraform',prepare),(reference,'resolve_terraform_modules',resolve),(facade,'resolve_terraform_modules',resolve),(reference,'refresh_terraform_paths',refresh)]:
            stack.enter_context(patch.object(mod,name,fn))
        stack.enter_context(patch.dict(facade._DISPATCH,{k:extract for k,v in facade._DISPATCH.items() if v is original}))
        stack.enter_context(patch.object(registry,'_REGISTRY',[dataclasses.replace(r,resolve=resolve) if r.resolve is original_resolve else r for r in registry._REGISTRY]))
        stack.enter_context(in_process_graphify())
        tests=upstream_tests('tests/test_terraform.py','tests/test_terraform_modules.py')
    rng=random.Random(2026092327)
    samples=['','# empty\n','x=1\ny=[]','resource "x" "y" { a=000123\nb=1e3\nc=-0\nd=0.0 }',
      'resource "x" "y" { a=1844674407370955161612345678901234567890\nb=-9223372036854775809123456 }',
      'resource "x" "y" { a={true=1, false=2, null=3, 42=4, 2.5=5}\nb={key={secret="test",not_secret="x"}}\nc=[{password="in-list"}] }',
      'module "x" { source="./$${literal}"\n v = "${var.name}" }\nlocals { a=module.x.id\nb=local.a }',
      'resource "x" "y" { a='+json.dumps('a\x00\x01\n\x7f'+'😀'*530)+' }',
      'resource "x" "y" { a='+('['*9)+'1'+(']'*9)+' }',
      'resource "x" "y" { a=['+','.join(str(i) for i in range(100))+']\nb={'+','.join(f'v{i}={i}' for i in range(100))+'} }',
      'resource "x" "y" { '+ '\n'.join(f'v{i}={i}' for i in range(150))+' }',
      'resource "same" "x" { a=1 }\nresource "same" "x" { a=2 }',
      'module "x" { source = "./x"\nsource = "../y" }',
      # PR #3762: secrets nested in list(object) defaults are redacted; scalar lists are untouched.
      'variable "x" { default = { configs = [{ password = "hunter2", name = "a" }], tags = ["a", "b"] } }\nresource "r" "s" { api_keys = ["k1", "k2"]\nplain = [1, 2] }']
    values=['true','false','null','123','-12','3.14','1e15','1e-15','"plain"','"${var.x}"','var.x','data.kind.name.id','module.x.name','aws_instance.web.id','local.a','count.index','each.value','self.attr','path.module','terraform.workspace','[var.x, local.a, module.x.id]','{password="demo", key=var.x}','[for x in var.items : x.id]','<<-EOF\nhello ${var.x}\nEOF\n']
    # Independently checked redaction contracts: labels and name/value pairs
    # carry secret signals that generic default/value keys do not.
    samples.extend([
        'resource "x" "y" { environment = [{ name = "DB_PASSWORD", value = "fixture-secret" }, { Name = "API_KEY", valueFrom = "fixture-ref" }, { name = "ACCESS_TOKEN", value_from = "fixture-under-ref" }, { name = "APP_MODE", value = "production" }] }',
        'variable "db_password" { default = "fixture-default" }\noutput "admin_token" { value = "fixture-output" }\nvariable "region" { default = "us-east-1" }',
        'variable "material" {\n sensitive = true\n default = "fixture-sensitive"\n}\noutput "other" {\n sensitive = "true"\n value = "fixture-output"\n}',
    ])
    for _ in range(360):
        blocks=[]
        for i in range(rng.randrange(1,7)):
            header=rng.choice(['resource "aws_instance" "web"','data "aws_ami" "ubuntu"','module "app"','variable "x"','output "result"','provider "aws"','locals','terraform','unknown "x"'])
            attrs='\n'.join(f'{rng.choice(["depends_on","password","apiKey","naïve","a","default","value","source"])} = {rng.choice(values)}' for _ in range(rng.randrange(7)))
            blocks.append(header+' {\n'+attrs+'\n}')
        text='\n'.join(blocks)
        if rng.random()<.15:text=text[:rng.randrange(len(text))]
        samples.append(text)
    with tempfile.TemporaryDirectory(prefix='graphify-terraform-') as tmp:
        root=Path(tmp).resolve();path=root/'project/main.tf';path.parent.mkdir()
        for text in samples:path.write_text(text);extract(path)
        for raw in [b'\xffresource "x" "y" {}',b'\xef\xbb\xbfmodule "x" {}',b'resource "x" "y" {\x00 a=1 }']:
            path.write_bytes(raw);extract(path)
        for name in ['main.hcl','main.tfvars','MAIN.TF','../outside.tf']:
            p=path.parent/name;p.write_text(samples[6]);extract(p)
        extract(path.parent/'missing.tf');extract(path.parent)
        saved=os.getcwd()
        try:os.chdir(root);extract(Path('project/main.tf'))
        finally:os.chdir(saved)
        for literal,message in [('1.0e309','Non-finite'),('"\\ud800"','surrogate'),('"\\udfff"','surrogate')]:
            path.write_text('resource "x" "y" { a='+literal+' }');expected=oracle.extract_terraform(path)
            assert expected['nodes'][1]['attributes']['a'] is not None
            response=native.call('extract.terraform',path=str(path));assert not response['ok'] and message in response['error'],response
        for i in range(200):
            directory=root/rng.choice(['project','project/child','outside','A-b','a_b']);directory.mkdir(parents=True,exist_ok=True)
            source=rng.choice(['./child','../outside','./missing','../../escape','https://remote.invalid','./'])
            p=directory/'main.tf';p.write_text(f'module "app" {{ source = "{source}" }}\nvariable "x" {{}}')
            result=extract(p);prepare(result,p,root if i%3 else root/'project')
        target=root/'target';target.mkdir();(root/'project/alias').symlink_to(target,target_is_directory=True)
        for boundary in [root,root/'project',Path('.')]:
            path.write_text('module "a" { source="./alias" }');result=extract(path);prepare(result,path,boundary)
    for i in range(400):
        directory=''.join(rng.choices(list('ABCabc012/-_.')+['é','İ','\n'],k=rng.randrange(40)))
        compare('terraform.scope',oracle._scope_id(directory),directory=directory)
        key=rng.choice(['db_password','APIKEY','AUTHORIZATION','apiKey','ſecret','clıent_secret','credentİal','not_sensitive','list','plain'])
        value=rng.choice(['value',None,42,True,{'password':'fake','inner':{'auth':'fake'},'list':[{'secret':'fake'}]},[{'password':'fake'}]])
        compare('terraform.redact',oracle._redact_value(key,value),key=key,value=value)
    options=['main.tf','dir/a.tf','x.TF','a.tfvars','.gitignore','.graphifyignore','./a.tf','a/../a.tf','a.py','.tf','root/.tf']
    for i in range(350):
        args=[[Path(rng.choice(options)) for _ in range(rng.randrange(10))] for _ in range(3)]
        refresh(*args)
    for i in range(300):
        per_file=[];nodes=[];edges=[]
        for j in range(rng.randrange(8)):
            directory=rng.choice(['.','a','b','A-b','a_b','a/sub']);source=f'{directory}/{j}'+rng.choice(['.tf','.hcl','.tfvars'])
            file={'id':f'file{j}','type':'terraform_file','source_file':source,'_terraform_directory':directory}
            call={'id':f'call{j}','module_source':'./x','module_source_location':'L3','source_file':source,'_terraform_directory':directory,'_terraform_source_directory':rng.choice(['.','a','b','missing'])}
            nodes.extend([file,call])
            if rng.random()<.65:per_file.append({'nodes':[file,call]})
        if rng.random()<.5:nodes.append({'id':'custom-anchor','type':'module','language':'terraform','_terraform_directory':'a','source_file':'a/old.tf'})
        resolve(per_file,nodes,edges)
finally:native.close()
grammar=Language(tree_sitter_hcl.language())
native.receipt('graphify/extractors/terraform.py',['src/extractors/terraform.rs','vendor/tree-sitter-hcl/src/lib.rs','vendor/tree-sitter-hcl/build.rs','vendor/tree-sitter-hcl/grammar/parser.c','vendor/tree-sitter-hcl/grammar/scanner.c','vendor/tree-sitter-hcl/SOURCE.json'],extraction_cases=cases,helper_comparisons=helpers,upstream_tests=tests,
    grammar={'python_package':importlib.metadata.version('tree-sitter-hcl'),'native_dependency':'exact 1.2.0 sdist grammar; SOURCE.json pins files','node_kinds':grammar.node_kind_count,'parse_states':grammar.parse_state_count,'abi':grammar.abi_version},
    comparisons='Exact extraction, direct-attribute limits/redaction, expressions, source escapes, portable directory IDs, source boundaries, existing anchors, fresh-node ownership, cached and incremental topology.',
    adaptations=['The CLI/watch fixture driver and generic corpus merge remain Python reference orchestration. Terraform extraction/preparation/resolution/refresh execute in Rust.','Non-finite numbers and unpaired Unicode surrogates are rejected explicitly at the JSON/UTF-8 graph boundary; no approximate value is persisted. Arbitrary-size JSON integers are preserved.','Terraform 0.9.70 redaction recurses through arrays and protects named secret values (value, valueFrom and value_from), secret-named variable/output blocks and literal sensitive=true markers. This is heuristic redaction, not a claim of complete secret scanning.'])
