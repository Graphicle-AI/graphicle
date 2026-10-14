#!/usr/bin/env python3
"""Qualify MCP configuration indexing without contacting configured servers."""
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
import graphify.mcp_ingest as reference
import graphify.extract as facade

spec=importlib.util.spec_from_file_location('_graphify_mcp_oracle',SOURCE/'graphify/mcp_ingest.py')
oracle=importlib.util.module_from_spec(spec);spec.loader.exec_module(oracle)
native=Native('mcp_ingest');cases=helpers=0;json_error_differences=[]
@functools.wraps(reference.extract_mcp_config)
def extract(path):
    global cases
    native.use_working_directory(os.getcwd());expected=oracle.extract_mcp_config(path)
    actual=native.result('mcp_config.extract',path=str(path));cases+=1
    if actual!=expected:
        if expected.get('error','').startswith('mcp_ingest json error:') and actual.get('error','').startswith('mcp_ingest json error:'):
            assert expected['nodes']==actual['nodes']==[] and expected['edges']==actual['edges']==[]
            json_error_differences.append({'expected':expected['error'],'actual':actual['error']})
        else:native.fail(path=str(path),expected=expected,actual=actual)
    return actual
def is_path(path):
    global helpers
    expected=oracle.is_mcp_config_path(path);actual=native.result('mcp_config.is_path',path=str(path));helpers+=1
    if actual!=expected:native.fail(operation='is_path',path=str(path),expected=expected,actual=actual)
    return actual
def package(args):
    global helpers
    expected=oracle._detect_package_from_args(args);actual=native.result('mcp_config.package',args=args);helpers+=1
    if actual!=expected:native.fail(operation='package',args=args,expected=expected,actual=actual)
    return actual
def strip(value):
    global helpers
    expected=oracle._strip_version(value);actual=native.result('mcp_config.strip_version',package=value);helpers+=1
    if actual!=expected:native.fail(operation='strip_version',value=value,expected=expected,actual=actual)
    return actual
try:
    actual=frozenset(native.result('mcp_config.filenames'))
    assert actual==oracle.MCP_CONFIG_FILENAMES
    with ExitStack() as stack:
        for module in [reference,facade]:
            for name,fn in [('extract_mcp_config',extract),('is_mcp_config_path',is_path)]:
                if hasattr(module,name):stack.enter_context(patch.object(module,name,fn))
        stack.enter_context(patch.object(reference,'MCP_CONFIG_FILENAMES',actual))
        stack.enter_context(patch.object(reference,'_detect_package_from_args',package))
        stack.enter_context(patch.object(reference,'_strip_version',strip))
        tests=upstream_tests('tests/test_mcp_ingest.py')
    rng=random.Random(2026092333)
    specs=[{}, {'command':'npx','args':['-y','@scope/pkg@2.0','/local/path'],'env':{'KEY':'PRIVATE_VALUE'}},
        {'command':'\x1f uvx \x1f','args':['mcp-server-fetch','--x'], 'env':{'KEY':'secret','OTHER':{'nested':'secret'}}},
        {'command':42,'args':[None,42,False,'--yes','server-mcp'], 'env':[]},
        {'command':'','args':'mcp-server-fetch','env':None},
        {'command':'<script>x</script>\x00','args':['@s/p@é'],'env':{'\x00KEY\x1f':'secret'}},
        {'url':'https://example.com/mcp','headers':{'Authorization':'SECRET_HEADER'}}]
    with tempfile.TemporaryDirectory(prefix='graphify-mcp-') as tmp:
        root=Path(tmp);path=root/'.mcp.json'
        for doc in [None,[],True,5,'x',{}, {'mcp':{'servers':{}}}, {'mcpServers':[],'mcp':{'servers':{'a':specs[1]}}},
                    {'mcpServers':{str(i):specs[i%len(specs)] for i in range(240)}}]:
            path.write_text(json.dumps(doc));extract(path)
        for _ in range(650):
            entries={rng.choice(['a','A','a!','a?','é','🙂','','<b>x</b>'])+str(rng.randrange(8)):rng.choice(specs+[None,False,3,[], 'x']) for _ in range(rng.randrange(15))}
            doc={'mcpServers':entries} if rng.random()<.7 else {'mcp':{'servers':entries}}
            path.write_text(json.dumps(doc,ensure_ascii=False));result=extract(path)
            assert 'PRIVATE_VALUE' not in json.dumps(result)
            assert 'SECRET_HEADER' not in json.dumps(result)
        for raw in [b'',b'{',b'{"mcpServers":}',b'{"mcpServers":{},}',b'\xef\xbb\xbf{}',b'{} trailing',b'{"a":"bad\ncontrol"}',
                    b'{"mcpServers":{},"mcpServers":{"a":{}}}',b'\xff',b'a\xe2',b'\xe2\x82x',b'\xf0\x90\x80',b'\xed\xa0\x80',b'x'*1_048_577]:
            path.write_bytes(raw);extract(path)
        extract(root/'missing');extract(root)
        path.write_text('{"mcpServers":{"local":{"command":"npx"}}}')
        saved=os.getcwd()
        try:os.chdir(root);extract(Path('.mcp.json'))
        finally:os.chdir(saved)
    args=['-y','--yes','@scope/name','@scope/name@1.2.3','@scope/name@é','mcp-server-fetch','server-mcp','a-mcp-extra','other','--value=secret','/tmp/file',None,False,1,'',' \x1f @s/name \n']
    for _ in range(1000):package(rng.choices(args,k=rng.randrange(9)))
    for _ in range(500):
        value=''.join(rng.choices('Aaz09@/.-_é🙂\n',k=rng.randrange(50)));strip(value)
        is_path(Path(rng.choice(['/a/', './', ''])+rng.choice(['.mcp.json','MCP.JSON','mcp.json','claude_desktop_config.json','mcp_servers.json','package.json','é.json'])))
finally:native.close()
native.receipt('graphify/mcp_ingest.py',['src/mcp_ingest.rs'],extraction_cases=cases,helper_cases=helpers,upstream_tests=tests,
    native_json_diagnostics=json_error_differences,
    comparisons='Exact server/config IDs, shared command/package/env-name nodes, nested map fallback, labels, package matching, server cap, file cap, missing-file/UTF-8 errors and source locations.',
    adaptations=['Malformed JSON retains the error category and empty graph, with native JSON parser diagnostic wording.',
        'The native JSON boundary rejects nonfinite numeric extensions and unpaired UTF-16 surrogates; qualification uses standard UTF-8 JSON.',
        'Original extraction dispatch remains the Python test orchestrator; MCP parsing itself is native and does not start any configured server.'])
