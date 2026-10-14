#!/usr/bin/env python3
"""Run unchanged SCIP ingestion contracts against native APIs and an isolated oracle."""
import functools
import importlib.util
import random
from contextlib import ExitStack
from unittest.mock import patch
from parity import Native,SOURCE,upstream_tests
import graphify.scip_ingest as reference

spec=importlib.util.spec_from_file_location('_graphify_scip_oracle',SOURCE/'graphify/scip_ingest.py')
oracle=importlib.util.module_from_spec(spec);spec.loader.exec_module(oracle)
native=Native('scip_ingest');cases=helpers=0
def compare(op,expected,**request):
    actual=native.result(op,**request)
    if actual!=expected:native.fail(operation=op,request=request,expected=expected,actual=actual)
    return actual
@functools.wraps(reference.ingest_scip_json)
def ingest(doc,source_file='',language='python'):
    global cases
    cases+=1
    return compare('scip.ingest',oracle.ingest_scip_json(doc,source_file,language),doc=doc,source_file=source_file,language=language)
def helper(name,operation,fields,transform=None):
    original=getattr(oracle,name)
    @functools.wraps(original)
    def run(*args):
        global helpers
        helpers+=1
        data=dict(zip(fields,args))
        if transform:data=transform(data)
        return compare(operation,original(*args),**data)
    return run
helpers_map={
 '_make_scip_node_id':helper('_make_scip_node_id','scip.node_id',['symbol','source_file']),
 '_build_scip_metadata':helper('_build_scip_metadata','scip.metadata',['symbol','kind','description']),
 '_scip_kind_to_file_type':helper('_scip_kind_to_file_type','scip.kind',['kind']),
 '_first_occurrence_line':helper('_first_occurrence_line','scip.first_line',['occurrences']),
 '_scip_relation_for':helper('_scip_relation_for','scip.relation',['relationship']),
 '_coerce_str':helper('_coerce_str','scip.coerce',['value','default']),
 '_is_true':helper('_is_true','scip.is_true',['value']),
 '_resolve_relationship_target':helper('_resolve_relationship_target','scip.resolve',['target','source_file','per_doc','global'],
      lambda d:{**d,'per_doc':[[k[0],k[1],v] for k,v in d['per_doc'].items()]})}
try:
    with ExitStack() as stack:
        stack.enter_context(patch.object(reference,'ingest_scip_json',ingest))
        for name,fn in helpers_map.items():stack.enter_context(patch.object(reference,name,fn))
        tests=upstream_tests('tests/test_scip_ingest.py')
    rng=random.Random(2026092332)
    scalars=[None,True,False,0,1,-1,1.0,2.5,'','false','true','foo','é🙂',[],{},['x']]
    for value in scalars:ingest(value)
    for _ in range(750):
        documents=[]
        for _ in range(rng.randrange(1,5)):
            symbols=[]
            for _ in range(rng.randrange(1,7)):
                name=rng.choice(['A#','B#','C#run()','lib#x!@y','same','<script>#bad','🙂#é',''])
                symbol={'symbol':name,'kind':rng.choice(['class','function',None,True]),
                    'display_name':rng.choice(['','named',None,[],'<b>name</b>']),
                    'documentation':rng.choice([None,[],[None],['line\x00\x1f<script> & é'],['é'*700]]),
                    'occurrences':rng.choice([[],None,[{}],[{'range':[rng.choice([-1,0,1,9,True,'3',2.5,10**80]),2,3]}]]),
                    'relationships':[{'symbol':rng.choice(['A#','B#','C#run()','same','external#','',None]),
                        'is_implementation':rng.choice(scalars), 'is_type_definition':rng.choice(scalars),
                        'is_definition':rng.choice(scalars), 'details':rng.choice(scalars)} for _ in range(rng.randrange(5))]}
                if rng.random()<.1:symbol=rng.choice(scalars)
                symbols.append(symbol)
            document={'relative_path':rng.choice(['a.py','b.py','nested/a.py','',None,False]),
                'language':rng.choice(['python','rust',None,7]),'symbols':symbols}
            if rng.random()<.1:document=rng.choice(scalars)
            documents.append(document)
        ingest({'documents':documents},rng.choice(['fallback.py',None,42]),rng.choice(['python','rust',None]))
    for _ in range(600):
        symbol=''.join(rng.choices('Ab.#_()!? é🙂日本語',k=rng.randrange(80)))
        source=''.join(rng.choices('src/Ab._é🙂',k=rng.randrange(40)))
        helpers_map['_make_scip_node_id'](symbol,source)
        helpers_map['_build_scip_metadata'](symbol,source,rng.choice(['','desc','<b>x</b>\x00']))
        helpers_map['_coerce_str'](rng.choice(scalars),rng.choice(scalars))
        helpers_map['_is_true'](rng.choice(scalars))
    for line in scalars+[10**100,2**64,2**63,-10**100]:
        helpers_map['_first_occurrence_line']([{'range':[line]}])
    for _ in range(600):
        per={(rng.choice('abc'),rng.choice('xyz')):rng.choice(['id1','id2','id3']) for _ in range(5)}
        glob={c:rng.sample(['id1','id2','id3'],rng.randrange(4)) for c in 'abc'}
        helpers_map['_resolve_relationship_target'](rng.choice('abcd'),rng.choice('xyz'),per,glob)
        helpers_map['_scip_relation_for']({k:rng.choice(scalars) for k in ['is_reference','is_implementation','is_type_definition','is_definition']})
finally:native.close()
native.receipt('graphify/scip_ingest.py',['src/scip_ingest.rs'],ingestion_cases=cases,helper_cases=helpers,upstream_tests=tests,
    comparisons='Exact native symbol IDs, same-document resolution and ambiguous external stubs, canonical records, strict boolean flags, large integer line values, deduplication and sanitized metadata.',
    adaptations=['This preserves the original simplified SCIP JSON shape. It is not official SCIP protobuf ingestion.',
        'The original build integration test uses the Python build_from_json consumer with native ingestion output.'])
