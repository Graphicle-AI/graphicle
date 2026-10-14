#!/usr/bin/env python3
"""Pinned metadata comparisons and real-process gws command fixtures, no account access."""
import json, os, random, sys, tempfile
from pathlib import Path
from parity import Native, upstream_tests
import graphify.google_workspace as gw

with tempfile.TemporaryDirectory() as temporary:
    root=Path(temporary);bin_dir=root/'bin';bin_dir.mkdir()
    control=root/'control.json';log=root/'invocation.json'
    # This executable is a development transport fixture, never shipped or invoked
    # by the application. It captures the actual subprocess argv/cwd and writes
    # provider bytes where gws would put them.
    fake=bin_dir/'gws'
    fake.write_text('#!'+sys.executable+'\n'+'''
import json,os,sys,time
from pathlib import Path
control=json.loads(Path(os.environ['GRAPHIFY_GWS_FIXTURE_CONTROL']).read_text())
Path(os.environ['GRAPHIFY_GWS_FIXTURE_LOG']).write_text(json.dumps({'argv':sys.argv[1:],'cwd':os.getcwd()}))
time.sleep(control.get('delay',0))
if control.get('returncode',0):
    sys.stdout.write(control.get('stdout',''));sys.stderr.write(control.get('stderr',''));sys.exit(control['returncode'])
Path(sys.argv[sys.argv.index('-o')+1]).write_bytes(control.get('body','').encode())
''');fake.chmod(0o755)
    os.environ['PATH']=str(bin_dir)+os.pathsep+os.environ['PATH']
    os.environ['GRAPHIFY_GWS_FIXTURE_CONTROL']=str(control);os.environ['GRAPHIFY_GWS_FIXTURE_LOG']=str(log)
    control.write_text(json.dumps({'body':'# Planning\n\nExported doc text.\n'}))
    native=Native('google_workspace');comparisons=0;exports=0
    originals={name:getattr(gw,name) for name in ('read_google_shortcut','google_workspace_enabled')}
    def call(method,**kwargs): return native.result('google_workspace',method=method,**kwargs)
    def same(method,expected,**kwargs):
        global comparisons
        got=call(method,**kwargs);comparisons+=1
        if got!=expected:native.fail(method=method,kwargs=kwargs,expected=expected,actual=got)
    try:
        gw.read_google_shortcut=lambda p:call('read',path=str(p))
        gw.google_workspace_enabled=lambda value=None:call('enabled',value=value if value is not None else os.environ.get('GRAPHIFY_GOOGLE_WORKSPACE',''))
        tests=upstream_tests(*['tests/test_google_workspace.py::'+name for name in ('test_read_google_shortcut_doc_id','test_read_google_shortcut_extracts_id_from_url','test_google_workspace_enabled_env')])
        for name,value in originals.items():setattr(gw,name,value)
        values=['','yes','true','YES',' off ','1','0','on','no','\x1cyEs\x1f']
        for value in values:same('enabled',gw.google_workspace_enabled(value),value=value)
        urls=['','https://drive.google.com/open?id=doc-1','https://drive.google.com/open?id=&id=second','https://drive.google.com/open?id=A%2FB+space','https://drive.google.com/open?id=%FF','/document/d/doc-id/edit?resourcekey=key#part','https://drive.google.com/file/d/f-1/view;param?id=query-id','https://docs.google.com/spreadsheets/d/sheet-id/edit','https://docs.google.com/presentation/d/slide-id/edit','https://docs.google.com/invalid']
        for url in urls:
            same('file_id',gw._extract_file_id_from_url(url),url=url)
            for data in ({},{'resource_key':'explicit'},{'resourceKey':123},{'resource_key':'','resourceKey':True}):same('resource_key',gw._extract_resource_key(url,data),url=url,data=data)
        for index,data in enumerate([{'doc_id':'a'},{'file_id':24},{'fileId':'c'},{'id':'d','doc_id':'preferred'},{'resource_id':'document:fallback'},{'url':urls[1],'email':'person@example.test'}, {'url':urls[5],'resourceKey':'explicit'}]):
            path=root/f'metadata-{index}.gdoc';path.write_text(json.dumps(data))
            same('read',gw.read_google_shortcut(path),path=str(path))
        for name in ['Café.gdoc','Cafe\u0301.gdoc','quote"slash\\.gsheet','日本語.gslides']:
            path=root/'workspace'/name;out=root/'workspace'/'graphify-out'/'converted'
            same('sidecar',str(gw._sidecar_path(path,out,root=root/'workspace')),path=str(path),out_dir=str(out),root=str(root/'workspace'))
            shortcut={'file_id':'doc"id','url':'https://docs.google.com/document/d/doc-id/edit','resource_key':None,'account':'person@example.test'}
            same('frontmatter',gw._with_frontmatter(path,shortcut,'\r\n body \n','text/markdown'),path=str(path),shortcut=shortcut,body='\r\n body \n',mime='text/markdown')
        # Successful and empty exports go through the actual command runner.
        for ext,mime in [('gdoc','text/markdown'),('gslides','text/plain')]:
            path=root/f'Planning.{ext}';path.write_text(json.dumps({'doc_id':'doc-123','resource_key':'not-a-query-param','email':'person@example.test'}))
            for index,body in enumerate(['# Planning\n\nExported doc text.','  \n\r  ','Unicode: 日本語\r\nline two']):
                control.write_text(json.dumps({'body':body}))
                out=root/f'converted-{ext}-{index}'
                expected=gw.convert_google_workspace_file(path,out,root=root)
                expected_text=expected.read_text() if expected else None
                actual=call('convert',path=str(path),out_dir=str(out),root=str(root));exports+=1
                assert actual==(str(expected) if expected else None)
                assert (Path(actual).read_text() if actual else None)==expected_text
                assert list(out.glob('*.txt'))==[] and all(not p.name.startswith('.tmp') for p in out.iterdir())
                invocation=json.loads(log.read_text());argv=invocation['argv']
                assert argv[:3]==['drive','files','export']
                assert json.loads(argv[argv.index('--params')+1])=={'fileId':'doc-123','mimeType':mime}
                assert Path(invocation['cwd'])==out.resolve() and '/' not in argv[-1]
        for stderr,stdout in [('denied',''),('', 'fallback output'),('x'*1201,''),('é'*1201,'')]:
            control.write_text(json.dumps({'returncode':1,'stderr':stderr,'stdout':stdout}))
            path=root/'failure.md'
            try:gw._run_gws_export('bad','text/markdown',path)
            except RuntimeError as error:expected=str(error)
            else:raise AssertionError('reference did not fail')
            result=native.call('google_workspace',method='export',file_id='bad',mime='text/markdown',path=str(path));exports+=1
            assert not result['ok'] and result['error']==expected,(result,expected)
        native.receipt('graphify/google_workspace.py',['src/google_workspace.rs'],original_tests=tests,metadata_comparisons=comparisons,actual_subprocess_cases=exports,
            native_callback_test='google_workspace::tests::sheet_conversion_invokes_native_converter_and_cleans_temporary_export',
            adaptations=['Uses the official gws executable via argv, cwd and timeout, with no shell interpolation or embedded Python runtime. Qualification uses a local executable transport fixture; no real Google account is exercised.',
            'Sheets accept a native XLSX callback. The callback and cleanup-on-error are exercised in Rust; discover/office wiring is tracked in detect.py.',
            'Rust JSON/read errors and missing-converter installation guidance use native error wording. Successful metadata, sidecar names, generated contents and provider error truncation match the reference.'])
    finally:
        for name,value in originals.items():setattr(gw,name,value)
        native.close()
