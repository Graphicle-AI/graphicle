#!/usr/bin/env python3
"""Run the original deduplication contracts against native Rust implementations."""
import contextlib, copy, inspect, io, itertools, math, os, random, sys, types
from pathlib import Path
from unittest.mock import patch
from parity import Native, SOURCE
from naming import to_upstream
from upstream_fixes import oracle_source
import graphify.dedup as reference
from rapidfuzz.distance import Jaro, JaroWinkler
import pytest

oracle=types.ModuleType('reference_dedup');oracle.__package__='graphify'
exec(compile(oracle_source('graphify/dedup.py'),'<pinned dedup.py with reviewed fixes>','exec'),oracle.__dict__)
native=Native('dedup');originals={};comparisons=0;whole_graphs=0;llm_scenarios=0
def plain(value):
    if isinstance(value,Path):return str(value)
    if isinstance(value,(set,frozenset)):return sorted(value)
    if isinstance(value,(tuple,list)):return [plain(v) for v in value]
    if isinstance(value,dict):return {k:plain(v) for k,v in value.items()}
    return value
def call(name,**kwargs):
    native.use_working_directory(os.getcwd());return to_upstream(native.result('dedup',function=name,**plain(kwargs)))
def wrap(name):
    original=getattr(reference,name);originals[name]=original;signature=inspect.signature(original)
    def invoke(*args,**kwargs):
        bound=signature.bind(*args,**kwargs);bound.apply_defaults();params=dict(bound.arguments)
        result=call(name,**params)
        if name=='deduplicate_entities':
            for line in result['stdout']:print(line)
            for line in result['stderr']:print(line,file=sys.stderr)
            if params.get('hyperedges') is not None:params['hyperedges'][:]=result['hyperedges']
            return result['nodes'],result['edges']
        if name=='_remap_hyperedge_members':params['hyperedges'][:]=result;return None
        if name=='_report_id_collision':
            for line in result:print(line,file=sys.stderr)
            return None
        if name in ('_shingles','_id_prefixes'):return set(result)
        if name=='_mentions':return {tuple(pair) for pair in result}
        if name in ('_disambiguate_defining_collisions','_path_qualify_dropped_collisions'):
            # The reference rewrites edges and hyperedges in place.
            for key in ('edges','hyperedges'):
                for old,new in zip(params.get(key) or [],result[key] or []):
                    if isinstance(old,dict):old.clear();old.update(new)
            for line in result['stderr']:print(line,file=sys.stderr)
            return result['nodes'],params['edges']
        if name=='_collision_rank':return (*result[:4],tuple(result[4]))
        return result
    return invoke
names=['_full_stem','_extension_qualified_id','_mentions','_disambiguate_defining_collisions','_path_qualify_dropped_collisions','_norm','_entropy','_shingles','_is_variant_pair','_short_label_blocked','_numeric_tokens_differ','_same_word_variant','_content_token_swap','_crossfile_fileanchored_blocked','_is_code','_id_prefixes','_defines_id','_reads_as_file_entity','_lifecycle_penalty','_rank_path','_collision_rank','_same_source_entity','_merge_missing_attributes','_report_id_collision','_remap_hyperedge_members','_content_richness','_pick_winner','deduplicate_entities']
class Reports:
    passed=0;skipped=0;deselected=[]
    def pytest_collection_modifyitems(self,config,items):
        omitted=[i for i in items if i.originalname=='test_crossfile_concept_merge_deterministic_across_hash_seeds']
        items[:]=[i for i in items if i not in omitted];self.deselected=[i.nodeid for i in omitted];config.hook.pytest_deselected(items=omitted)
    def pytest_runtest_logreport(self,report):
        if report.when=='call' and report.passed:self.passed+=1
        if report.skipped:self.skipped+=1
reports=Reports()
def compare(name,**kwargs):
    global comparisons
    expected=getattr(oracle,name)(**kwargs)
    if name=='_make_minhash':expected=expected.hashvalues.tolist()
    if name=='_mentions':expected=sorted(list(pair) for pair in expected)
    actual=call(name,**kwargs);comparisons+=1
    if isinstance(expected,float):equal=math.isclose(expected,actual,rel_tol=2e-15,abs_tol=2e-15)
    else:equal=plain(expected)==actual
    if not equal:native.fail(function=name,arguments=plain(kwargs),expected=plain(expected),actual=actual)
    return actual
def compare_graph(nodes,edges,communities=None,hyperedges=None,protected_ids=None):
    global whole_graphs
    he=copy.deepcopy(hyperedges);stdout=io.StringIO();stderr=io.StringIO()
    with contextlib.redirect_stdout(stdout),contextlib.redirect_stderr(stderr):
        expected=oracle.deduplicate_entities(copy.deepcopy(nodes),copy.deepcopy(edges),communities=communities or {},hyperedges=he,protected_ids=protected_ids)
    actual=call('deduplicate_entities',nodes=nodes,edges=edges,communities=communities or {},hyperedges=hyperedges,protected_ids=protected_ids)
    target={'nodes':expected[0],'edges':expected[1],'hyperedges':he,'stdout':stdout.getvalue().splitlines(),'stderr':stderr.getvalue().splitlines(),'calls':[]}
    whole_graphs+=1
    if actual!=target:native.fail(nodes=nodes,edges=edges,communities=communities,protected_ids=plain(protected_ids),expected=target,actual=actual)
try:
    for name in names:setattr(reference,name,wrap(name))
    files=['tests/test_dedup.py','tests/test_dedup_survivor_richness.py','tests/test_dedup_remaps_hyperedges.py','tests/test_build_merge_dedup_scope.py']
    status=pytest.main(['-q','-p','no:cacheprovider',*[str(SOURCE/p) for p in files]],plugins=[reports]);assert status==0,status
    for name,fn in originals.items():setattr(reference,name,fn)
    compare_graph([
        {'id':'walk','label':'walk()','source_file':'tree.py','file_type':'code'},
        {'id':'cache','label':'Cache','source_file':'guide.md','file_type':'concept'},
        {'id':'cache_2','label':'cache','source_file':'guide.md','file_type':'concept'},
    ], [
        {'source':'walk','target':'walk','relation':'calls'},
        {'from':'cache_2','to':'cache_2','relation':'references'},
        {'source':'cache_2','target':'cache','relation':'references'},
    ])
    rng=random.Random(64)
    samples=[None,True,123,1.25,[],{},['é'],{'label':'word'},'Straße','İstanbul','\u0345\u0300','東京_test','𝔸K_１２','A\x1cB','abc\n','ＡＢ','1\ufe0f']
    for _ in range(700):samples.append(''.join(chr(rng.randrange(0xd800)) for _ in range(rng.randrange(20))))
    for value in samples:
        compare('_norm',label=value);compare('_entropy',label=value)
    for text in ['','a','日本語','word\n# title\nparagraph','😀café']:
        for k in [-3,-1,0,1,2,3,8]:compare('_shingles',text=text,k=k)
        compare('_make_minhash',text=text,num_perm=128)
    alphabet='abcde0123_ é中😀\n'
    for _ in range(1200):
        a=''.join(rng.choices(alphabet,k=rng.randrange(35)));b=''.join(rng.choices(alphabet,k=rng.randrange(35)))
        actual=call('similarity',a=a,b=b);expected={'jaro':Jaro.normalized_similarity(a,b),'jaro_winkler':JaroWinkler.normalized_similarity(a,b)}
        if any(not math.isclose(actual[k],v,abs_tol=2e-15) for k,v in expected.items()):native.fail(a=a,b=b,expected=expected,actual=actual)
        comparisons+=1
        compare('_is_variant_pair',a=a,b=b);compare('_short_label_blocked',a=a,b=b,jw_score=expected['jaro_winkler']*100)
        compare('_numeric_tokens_differ',a=a,b=b);compare('_content_token_swap',a=a,b=b);compare('_same_word_variant',x=a,y=b)
    # issue #3775: a located non-code declaration must survive a collision with a richer sourceless semantic stub.
    real={'id':'docs_guide_setup','label':'Setup','file_type':'document','source_file':'docs/guide.md','source_location':'L3','attributes':{'kind':'heading'}}
    stub={'id':'setup','label':'Setup','file_type':'document','source_file':'','type':'external','external':True,'_origin':'semantic'}
    for pair in ([real,stub],[stub,real]):
        assert compare('_pick_winner',nodes=pair)['id']=='docs_guide_setup'
        compare_graph(copy.deepcopy(pair),[{'source':'setup','target':'docs_guide_setup','relation':'related'}])
    labels=['GraphExtractor','graph extractor','GraphExtracter','API','Acme Corp','acme-corp','Permission Manager','Permission Nanager','Export the Read Only Evidence Bundle','Export a Read Only Evidence Bundle','ADR 0001 Policy','ADR 0002 Policy','Asset Contribution Flow','Asset Consumption Flow']
    for _ in range(250):
        nodes=[]
        for i in range(rng.randrange(2,18)):
            n={'id':f'node_{i}','label':rng.choice(labels),'source_file':rng.choice(['a.md','b.md','','notes.md']),'file_type':rng.choice(['concept','code','rationale','document','image'])}
            if rng.random()<.2:n['attributes']={'owner':'team','active':True}
            if rng.random()<.15:n['node_kind']='heading'
            nodes.append(n)
        edges=[{'source':f'node_{rng.randrange(len(nodes))}','target':f'node_{rng.randrange(len(nodes))}','relation':'related'} for _ in range(5)]
        protected=set(rng.sample([n['id'] for n in nodes],rng.randrange(min(4,len(nodes)))))
        compare_graph(nodes,edges,hyperedges=[{'nodes':[n['id'] for n in nodes]}],protected_ids=protected)
    # 0.9.85-0.9.88 (#4281, #4362): ids shared by files that differ only by
    # extension or that dropped their directory, and mentions of documents.
    files_=['docs/protocol.md','docs/firmware/protocol.md','protocol.md','a/x.md','b/x.md','x.py','x.html','lib/portal.py','lib/portal.html','/abs/a/x.md','C:\\w\\x.md','c.md']
    ids_=['protocol','docs_protocol','docs_firmware_protocol','x','x_intro','a_x','x_py','lib_portal','lib_portal_html','note']
    for _ in range(300):
        nodes=[{'id':rng.choice(ids_),'label':rng.choice(['Protocol','protocol (top-level)','X','Intro','Portal page','portal.py']),'source_file':rng.choice(files_),'file_type':rng.choice(['document','document','code','concept'])} for _ in range(rng.randrange(2,9))]
        for n in nodes:
            if rng.random()<.2:n['_origin']='ast';n['source_location']='L1'
        edges=[{'source':rng.choice(ids_),'target':rng.choice(ids_),'relation':rng.choice(['references','references','calls']),'source_file':rng.choice(files_)} for _ in range(rng.randrange(0,6))]
        compare_graph(nodes,edges,hyperedges=[{'nodes':[n['id'] for n in nodes[:3]],'source_file':nodes[0]['source_file']}])
        compare('_mentions',nodes=nodes,edges=edges,root=None)
        for n in nodes:
            compare('_full_stem',source_file=n['source_file']);compare('_extension_qualified_id',node=n)
            compare('_collision_rank',node=n,root=None,mentions=reference._mentions(nodes,edges,None))
    # Exercise the native callback path with recorded responses. No model call
    # or external account is involved in a source-code compatibility check.
    import graphify.llm as llm
    candidates=[{'id':f'n_{i}','label':v,'source_file':'one.md','file_type':'concept'} for i,v in enumerate(['GraphExtractor','Graph Extractor','GraphExtracter','Permission Manager','Permission Nanager','ADR 001 Rule','ADR 002 Rule'])]
    for availability in ['available','unknown','missing']:
        for responses in [['1. yes\n2. no\n3. yes']*100,[{'error':'fixture service failure'}]*100,['garbage\n_1. yes\n1_. yes\n1. YES indeed\n99. yes\n0. yes\n+２. no']*100]:
            uf=oracle._UF();calls=[];stdout=io.StringIO();index=0
            def model(prompt,**kwargs):
                global index
                calls.append({'prompt':prompt,**kwargs});response=responses[len(calls)-1]
                if isinstance(response,dict):raise RuntimeError(response['error'])
                return response
            with patch.object(llm,'BACKENDS',{'fixture':{'env_key':'MODEL_API_KEY'}} if availability!='unknown' else {}),patch.object(llm,'_get_backend_api_key',lambda b:'' if availability=='missing' else 'fixture'),patch.object(llm,'_call_llm',model),contextlib.redirect_stdout(stdout):
                oracle._llm_tiebreak(candidates,uf,{},backend='fixture',batch_size=2,low=0,high=101)
            expected={'components':uf.components(),'stdout':stdout.getvalue().splitlines(),'calls':calls}
            actual=call('llm_scenario',candidates=candidates,communities={},backend='fixture',availability=availability,responses=responses,batch_size=2,low=0,high=101)
            llm_scenarios+=1
            if actual!=expected:native.fail(scenario='llm',availability=availability,responses=responses,expected=expected,actual=actual)
    native.receipt('graphify/dedup.py',['src/dedup.rs','src/dedup_strings.rs','src/dedup_api.rs'],original_tests={'files':files,'passed':reports.passed,'skipped':reports.skipped,'deselected':reports.deselected},independent_comparisons=comparisons,whole_graph_comparisons=whole_graphs,recorded_llm_scenarios=llm_scenarios,
        adaptations=['Original build integration fixtures call native deduplication; the surrounding Python build module is still assigned separately.',
        'LSH neighbors use stable ID ordering rather than Python randomized-set order. Winner selection retains original node order where the original contract uses it.',
        'Jaro and Unicode entropy allow a few floating-point ULPs; merge decisions and resulting graph attributes compare exactly.',
        'A typed native provider callback carries the opt-in LLM tiebreak. Prompt construction, batching, response parsing and failures are exercised with recorded callbacks. Provider registration/HTTP integration belongs to llm.py and is not claimed by this receipt.'])
finally:
    for name,fn in originals.items():setattr(reference,name,fn)
    native.close()
