#!/usr/bin/env python3
"""Original reflection tests and source-derived file/clock/escape comparisons."""
import functools
import importlib.util
import inspect
import json
import math
import os
import random
import tempfile
from datetime import datetime,timezone,timedelta
from pathlib import Path
from contextlib import ExitStack
from unittest.mock import patch
from parity import Native,SOURCE,upstream_tests
from naming import native_copies,to_native,to_upstream
from upstream_cli import in_process_graphify
import graphify.reflect as reference

spec=importlib.util.spec_from_file_location('_graphify_reflect_oracle',SOURCE/'graphify/reflect.py')
oracle=importlib.util.module_from_spec(spec);oracle.__package__='graphify';spec.loader.exec_module(oracle)
native=Native('reflect');counts={};file_comparisons=0;expected_errors=[];surrogate_rejections=0
NOW=datetime(2026,9,23,8,35,15,123456,tzinfo=timezone.utc)
def encoded(value):
    if isinstance(value,Path):return str(value)
    if isinstance(value,datetime):return value.isoformat()
    if isinstance(value,set):return sorted(value)
    if isinstance(value,tuple):return [encoded(v) for v in value]
    if isinstance(value,list):return [encoded(v) for v in value]
    if isinstance(value,dict):return {k:encoded(v) for k,v in value.items()}
    return value
def wrapper(name,op):
    original=getattr(oracle,name);signature=inspect.signature(original)
    @functools.wraps(getattr(reference,name))
    def call(*args,**kwargs):
        global file_comparisons
        native.use_working_directory(os.getcwd())
        bound=signature.bind(*args,**kwargs);bound.apply_defaults()
        if 'now' in bound.arguments and bound.arguments['now'] is None:bound.arguments['now']=datetime.now(timezone.utc)
        request={key:encoded(value) for key,value in bound.arguments.items()}
        for old,new in [('s','text'),('date_str','date'),('out','lines')]:
            if old in request:request[new]=request.pop(old)
        counts[op]=counts.get(op,0)+1
        if name=='_render_bucket':
            expected=list(bound.arguments['out']);original(expected,bound.arguments['data'],bound.arguments['k'])
        else:
            try:expected=original(*bound.args,**bound.kwargs)
            except Exception as e:
                actual=to_upstream(native.call(op,**request))
                if actual.get('ok'):native.fail(operation=op,request=request,expected_error=repr(e),actual=actual)
                expected_errors.append((op,type(e).__name__,actual['error']));raise
        paths=[]
        if name=='reflect':
            paths.append(Path(expected[0]))
            if bound.arguments.get('graph_path') is not None:paths.append(Path(bound.arguments['graph_path']).parent/oracle.LEARNING_SIDECAR_NAME)
        if name=='write_learning_sidecar':paths.append(Path(expected))
        before={p:p.read_bytes() for p in paths if p.is_file()}
        # Sidecars beside the graph are read by each side under its own name.
        graph=bound.arguments.get('graph_path')
        inputs=[Path(graph).parent/n for n in ('.graphify_analysis.json','.graphify_labels.json','.graphify_root')] if graph else []
        with native_copies(*inputs):actual=to_upstream(native.result(op,**request))
        if name=='_render_bucket':bound.arguments['out'][:]=actual
        if encoded(actual)!=encoded(expected):
            if name=='_decay' and math.isclose(actual,expected,rel_tol=3e-15,abs_tol=1e-15):pass
            else:native.fail(operation=op,request=request,expected=encoded(expected),actual=actual)
        for p,body in before.items():
            # The engine writes its learning sidecar under its own name.
            file_comparisons+=1;written=p.with_name(to_native(p.name))
            if not written.is_file() or to_upstream(written.read_bytes().decode('utf-8')).encode('utf-8')!=body:native.fail(operation=op,path=str(written),expected=body.decode(),actual=written.read_text() if written.is_file() else None)
        if name=='reflect':return Path(actual[0]),actual[1]
        if name in ('write_learning_sidecar','_resolve_source_path'):return Path(actual) if actual is not None else None
        if name=='_load_known_nodes':return None if actual is None else set(actual)
        if name=='_build_id_label_maps':return tuple(actual)
        if name=='_parse_dt':return None if actual is None else datetime.fromisoformat(actual)
        if name=='_render_bucket':return None
        return actual
    return call
operations={
    'parse_memory_doc':'parse','_yaml_unescape':'unescape','load_memory_docs':'load','_load_node_community':'node_community',
    '_load_known_nodes':'known_nodes','_doc_community':'doc_community','_parse_dt':'parse_dt','_decay':'decay',
    'aggregate_lessons':'aggregate','_dedupe_by_question':'dedupe','_render_bucket':'render_bucket','render_lessons_md':'render',
    'lessons_fresh':'fresh','reflect':'run','_build_id_label_maps':'id_label_maps','_resolve_canonical_id':'resolve_id',
    '_resolve_source_path':'resolve_path','_content_hash':'hash','_code_fingerprint':'fingerprint',
    '_provenance_for':'provenance','build_learning_overlay':'overlay','write_learning_sidecar':'write_sidecar',
    'load_learning_overlay':'load_overlay','_is_stale':'is_stale'}
functions={name:wrapper(name,'reflect.'+op) for name,op in operations.items()}
try:
    with ExitStack() as stack:
        for name,fn in functions.items():stack.enter_context(patch.object(reference,name,fn))
        stack.enter_context(in_process_graphify())
        tests=upstream_tests('tests/test_reflect.py')
    rng=random.Random(2026092350)
    values=['','plain','quote"','back\\slash','line\nnext','\0','\x1f','\u2028','\u2029','\r\n','é中',r'\u0x41',r'\u1_23',r'\u０x41',r'\u0X_F',r'\x-0',r'\x+1',r'\x 1',r'\xZZ',r'\uZZZZ',r'\q',r'\uFFFF']
    for value in values:
        functions['_yaml_unescape'](value)
        for prefix in ['---\n','---\r\n',' ---\n','---broken\n','---']:
            for field in ['question','correction','contributor','outcome','source_nodes','unknown']:
                functions['parse_memory_doc'](prefix+field+': "'+value+'"\nsource_nodes: ["A", 12, "B\\\"C"]\n---\n')
    for _ in range(1200):
        value=''.join(rng.choices(['a','\\','"','x','u','0','1','4','f','n','t','L','_','+','-',' ','é'],k=rng.randrange(40)))
        functions['_yaml_unescape'](value)
        functions['parse_memory_doc']('---\nsource_nodes: ['+value+']\nquestion: "'+value+'"\n---\n')
    for value in [r'\uD800',r'\uDFFF']:
        result=native.call('reflect.unescape',text=value);assert not result['ok'] and 'surrogate' in result['error'];surrogate_rejections+=1
    dates=['','bad','2026-06-01','20260601','2026W231','2026-W23','2026-W23-1','2026W23','2026-06-01T12','2026-06-01T12.5',
        '2026-06-01T1230,4','2026-06-01T12:30:45.123456789','2026-06-01T12:30+00:61','2026-06-01T12:30+01:00:61',
        '2026-06-01T12:30+01.5','2026-06-01T12:30+00:00:00.5','2026-06-01T12:30Z','2026-06-01T12:30:45+05:30:00.5',
        '2026-06-01\x0012:30','2026-06-01T12:30+24:00','2026-06-01T12:30+23:60']
    for _ in range(1000):
        dt=NOW+timedelta(days=rng.randrange(-15000,15000),seconds=rng.randrange(86400),microseconds=rng.randrange(1000000))
        day=dt.strftime(rng.choice(['%Y-%m-%d','%Y%m%d','%GW%V%u','%G-W%V-%u']))
        time=dt.strftime(rng.choice(['%H','%H%M','%H%M%S','%H:%M','%H:%M:%S']))
        dates.append(day+rng.choice(['T',' ','🕒','\x00','x'])+time+rng.choice(['','.3',',12345678'])+rng.choice(['','Z','+05:30','-04','+010030.6']))
    for day in dates:
        functions['_parse_dt'](day)
        for half in [0,30,100000]:functions['_decay'](day,NOW,half)
    for _ in range(400):
        names=['A','B','C','D','é中','Call()','Ghost'];docs=[]
        for i in range(rng.randrange(50)):
            docs.append({'outcome':rng.choice([None,'useful','dead_end','corrected','unknown']),'source_nodes':rng.choices(names,k=rng.randrange(7)),
                'date':rng.choice(dates),'question':'Q'+str(rng.randrange(5)),'correction':rng.choice(['fixed','right',''])})
        mapping={name:rng.choice(['A','Z','Uncategorized']) for name in names[:4]} if rng.random()<0.5 else None
        known=set(names[:5]) if rng.random()<0.5 else None
        agg=functions['aggregate_lessons'](docs,mapping,now=NOW,half_life_days=rng.choice([0,1,30,3000]),min_corroboration=rng.choice([1,2,3,5]),known_nodes=known)
        functions['render_lessons_md'](agg)
        functions['_dedupe_by_question'](agg['dead_ends']+agg['corrections'])
        for name in names[:2]:functions['_provenance_for'](name,agg['_node_provenance'],'useful')
        lines=['preface'];functions['_render_bucket'](lines,agg,agg['min_corroboration'])
    with tempfile.TemporaryDirectory(prefix='graphify-reflect-') as tmp:
        root=Path(tmp);out=root/'graphify-out';out.mkdir();memory=out/'memory';memory.mkdir();gp=out/'graph.json'
        gp.write_text(json.dumps({'nodes':[{'id':'a','label':'A','source_file':'src.py'},{'id':'b','label':'B','source_file':'src.py'}]}))
        (root/'src.py').write_text('source\n');ap=out/'.graphify_analysis.json';lp=out/'.graphify_labels.json'
        ap.write_text(json.dumps({'communities':{'0':['a'],'2':['b']}}));lp.write_text(json.dumps({'0':'Alpha','2':'Beta'}))
        for i in range(20):
            (memory/f'{i:02}.md').write_text('---\ndate: "2026-06-01"\nquestion: "Q'+str(i)+'"\noutcome: "'+rng.choice(['useful','dead_end','corrected'])+'"\nsource_nodes: ["A", "A", "B"]\n---\n')
        (memory/'bad.md').write_bytes(b'\xff\x80');(memory/'foreign.md').write_text('foreign');(memory/'.hidden.md').write_text('---\nquestion: "hidden"\n---')
        functions['load_memory_docs'](memory);functions['_load_node_community'](gp,ap,lp);functions['_load_known_nodes'](gp);functions['_build_id_label_maps'](gp)
        result=functions['reflect'](memory,out/'reflections/LESSONS.md',gp,now=NOW)
        functions['load_learning_overlay'](gp);functions['_code_fingerprint']({'source_file':'src.py'},gp)
        functions['_resolve_source_path']('src.py',gp);functions['_content_hash'](root/'src.py')
        (root/'src.py').write_text('changed\n');functions['load_learning_overlay'](gp)
        (root/'src.py').unlink();functions['load_learning_overlay'](gp)
        for invalid in ['broken','[]','null','{"nodes":null}','{"nodes":[]}','{"nodes":{"a":null,"b":{}}}']:
            # Each side reads the learning sidecar under its own name.
            for name in {oracle.LEARNING_SIDECAR_NAME,to_native(oracle.LEARNING_SIDECAR_NAME)}:(out/name).write_text(invalid)
            try:functions['load_learning_overlay'](gp)
            except (AttributeError,TypeError):pass
    native.receipt('graphify/reflect.py',['src/reflect.rs','src/python_datetime.rs'],upstream_tests=tests,operations=counts,
        exact_written_file_comparisons=file_comparisons,expected_errors=expected_errors,surrogate_rejections=surrogate_rejections,
        adaptations=['Original Python CLI/save-result/build fixtures call native reflection; they are not claims of complete native CLI/ingestion.',
            'The native JSON/UTF-8 interface explicitly rejects unpaired surrogate outputs from YAML escapes.',
            'A shared clock instant is supplied to both implementations; aggregate rounded scores, lessons and sidecar bytes compare exactly. Raw decay permits a few floating-point ULPs.'])
finally:native.close()
