#!/usr/bin/env python3
"""Qualify native graph assembly and incremental replacement with original cases."""
import contextlib, copy, inspect, io, json, math, os, random, sys, tempfile, types
from pathlib import Path
from unittest.mock import patch
from parity import Native,SOURCE
from naming import to_upstream
import upstream_naming
from upstream_fixes import oracle_source
import graphify.build as reference
import graphify.security as security
import networkx as nx
import pytest

oracle=types.ModuleType('reference_build');oracle.__package__='graphify'
exec(compile(oracle_source('graphify/build.py'),'<pinned build.py with reviewed fixes>','exec'),oracle.__dict__)
native=Native('build');originals={};comparisons=0;graph_comparisons=0;native_cap=None
def plain(value):
    if isinstance(value,nx.Graph):return plain(nx.node_link_data(value,edges='edges'))
    if isinstance(value,Path):return str(value)
    if isinstance(value,float) and not math.isfinite(value):return str(value)
    if isinstance(value,(set,frozenset)):return sorted(plain(v) for v in value)
    if isinstance(value,(tuple,list)):return [plain(v) for v in value]
    if isinstance(value,dict):return {k:plain(v) for k,v in value.items()}
    return value
def assign(target,source):
    if isinstance(target,dict) and isinstance(source,dict):
        for key in list(target):
            if key not in source:del target[key]
        for key,value in source.items():
            if key in target and isinstance(target[key],(list,dict)) and type(target[key])==type(value):assign(target[key],value)
            else:target[key]=copy.deepcopy(value)
    elif isinstance(target,list) and isinstance(source,list):
        for i,value in enumerate(source):
            if i<len(target) and isinstance(target[i],(list,dict)) and type(target[i])==type(value):assign(target[i],value)
            elif i<len(target):target[i]=copy.deepcopy(value)
            else:target.append(copy.deepcopy(value))
        del target[len(source):]
def call(name,**kwargs):
    global native_cap
    cap=os.environ.get('GRAPHIFY_MAX_GRAPH_BYTES',str(security._MAX_GRAPH_FILE_BYTES))
    with patch.dict(os.environ,{'GRAPHIFY_MAX_GRAPH_BYTES':cap}):
        if cap!=native_cap:native.cwd=None;native_cap=cap
        native.use_working_directory(os.getcwd())
        result=to_upstream(native.call('build',function=name,**plain(kwargs)))
    if not result['ok']:
        message=result['error']
        if message.startswith('Cannot read ') or message.startswith('Stored '):raise RuntimeError(message)
        raise ValueError(message)
    return result['result']
def to_graph(data):return nx.node_link_graph(data,edges='edges')
def replace_graph(target,data):
    # Drop cached NetworkX views before adopting the native graph state.
    replacement=to_graph(data)
    target.__dict__.clear()
    target.__dict__.update(replacement.__dict__)
def wrap(name):
    original=getattr(reference,name);originals[name]=original;signature=inspect.signature(original)
    def invoke(*args,**kwargs):
        bound=signature.bind(*args,**kwargs);bound.apply_defaults();params=dict(bound.arguments)
        for key in ('new_chunks','chunks','extractions'):
            if key in params:params[key]=list(params[key])
        for key in ('ast_sources',):
            if params.get(key) is not None:params[key]=list(params[key])
        out=call(name,**params);result=out['value']
        for line in out['stdout']:print(line)
        for line in out['stderr']:print(line,file=sys.stderr)
        if name=='build_from_json':assign(params['extraction'],result['extraction']);return to_graph(result['graph'])
        if name in ('build','build_merge','prefix_graph_for_global'):return to_graph(result)
        if name=='_disambiguate_file_node_labels':replace_graph(params['G'],result);return None
        if name=='_mint_external_stub':replace_graph(params['G'],result);params['node_set'].update(params['G'].nodes);return None
        if name=='prune_repo_from_graph':replace_graph(params['G'],result['graph']);return result['count']
        mutations={'mint_external_stubs_in_data':'data','finalize_raw_graph_endpoints':'data','_backfill_origin':'item','_normalize_hyperedge_members':'he','_fold_node_aliases':'node','_fold_edge_aliases':'edge','_coerce_non_string_ids':'extraction','disambiguate_file_labels_in_nodes':'nodes','merge_raw_extraction':'new'}
        if name in mutations:assign(params[mutations[name]],result);return params['new'] if name=='merge_raw_extraction' else None
        if name=='_execution_wins':assign(params['a'],result[0]);assign(params['b'],result[1]);return None
        if name=='_tier_replacement_sources':return tuple(set(v) for v in result)
        if name=='_file_label_reassignments':return dict(result)
        if name in ('_build_prune_sets','deduplicate_by_label','_load_existing_graph'):return tuple(result) if result is not None else None
        return result
    return invoke
names=['_is_ast_tier','_is_external_stub','_backfill_origin','finalize_raw_graph_endpoints','_execution_wins','_lost_saved_semantic_nodes','_mint_external_stub','mint_external_stubs_in_data','_coerce_hyperedge_member_refs','_normalize_hyperedge_members','_fold_node_aliases','_fold_edge_aliases','_coerce_id','_hashable','_coerce_non_string_ids','_norm_source_file','_abs_identity','_is_file_node_label','_shortest_unique_suffix','_file_label_reassignments','_disambiguate_file_node_labels','disambiguate_file_labels_in_nodes','_infer_merge_root','_build_prune_sets','_derive_prune_root','edge_data','edge_datas','dedupe_nodes','dedupe_edges','_old_file_stems','_semantic_id_remap','_has_global_id','graph_has_legacy_ids','_doc_twin_remap','build_from_json','build','_norm_label','deduplicate_by_label','_load_existing_graph','_tier_replacement_sources','merge_raw_extraction','build_merge','prefix_graph_for_global','distinct_repo_tags','prune_repo_from_graph']
class Reports:
    passed=0;skipped=0;deselected=[]
    def pytest_collection_modifyitems(self,config,items):
        omitted=[i for i in items if i.originalname in {'test_unexplained_loss_blocked','test_dedup_skip_preserved','test_build_annotations_all_resolve'}]
        items[:]=[i for i in items if i not in omitted]
        self.deselected=[i.nodeid for i in omitted];config.hook.pytest_deselected(items=omitted)
    def pytest_runtest_logreport(self,report):
        if report.when=='call' and report.passed:self.passed+=1
        if report.skipped:self.skipped+=1
reports=Reports()
def compare(name,**kwargs):
    global comparisons,graph_comparisons
    original=copy.deepcopy(kwargs);stdout=io.StringIO();stderr=io.StringIO()
    try:
        with contextlib.redirect_stdout(stdout),contextlib.redirect_stderr(stderr):expected=getattr(oracle,name)(**original)
    except (ValueError,RuntimeError,TypeError) as error:
        try:call(name,**kwargs)
        except (ValueError,RuntimeError):comparisons+=1;return
        native.fail(function=name,arguments=plain(kwargs),expected_error=str(error))
    actual=call(name,**kwargs)['value'];comparisons+=1
    # In-place reference functions: compare the argument they rewrote.
    in_place={'finalize_raw_graph_endpoints':'data','_backfill_origin':'item'}
    if name in in_place:expected=original[in_place[name]]
    if name=='build_from_json':actual=actual['graph']
    if isinstance(expected,nx.Graph):graph_comparisons+=1
    if name=='_file_label_reassignments':expected=list(expected.items())
    if plain(expected)!=actual:native.fail(function=name,arguments=plain(kwargs),expected=plain(expected),actual=actual)
try:
    for name in names:setattr(reference,name,wrap(name))
    files=['tests/test_build.py','tests/test_build_merge_hyperedges_and_prune.py','tests/test_build_merge_shrink_guard.py','tests/test_hyperedge_member_shapes.py','tests/test_prune_sweeps_orphans.py','tests/test_file_label_disambiguation.py','tests/test_carried_hyperedge_remap.py','tests/test_issue_3472_source_file_collision.py','tests/test_build_merge_dedup_scope.py','tests/test_build_located_semantic_identity.py',
        # 0.9.80-0.9.88: stub endpoints, type-only folding, membership precedence,
        # lost semantic nodes and field aliases.
        'tests/test_external_stub_endpoints.py','tests/test_type_only_import_cycles.py','tests/test_relation_collapse_precedence.py','tests/test_csharp_object_creation.py','tests/test_lost_saved_semantic_nodes.py','tests/test_alias_folding.py',
        # 0.9.80 (#4189): the raw graph's endpoints match build_from_json; the
        # file's update/extract cases run end to end in check_watch.py.
        'tests/test_no_cluster_undeclared_endpoints.py::test_finalize_raw_graph_endpoints_matches_build_from_json',
        # First run here on 14 October 2026: non-string ids, the
        # semantic id remap root, merging over a corrupt graph.json, --no-dedup.
        'tests/test_non_string_node_ids.py','tests/test_semantic_id_remap_root.py','tests/test_corrupt_graph_json.py','tests/test_no_dedup_flag.py']
    with upstream_naming.isolated(files,'check_build') as (_,copies):status=pytest.main(['-q','-p','no:cacheprovider',*copies],plugins=[reports])
    assert status==0,status
    for name,fn in originals.items():setattr(reference,name,fn)
    rng=random.Random(65)
    for _ in range(250):
        paths=[rng.choice(['src/','pkg/','docs/',''])+rng.choice(['index','api','module','日本語'])+rng.choice(['.py','.ts','.md','.h','.cpp']) for _ in range(6)]
        for sf in paths:
            compare('_old_file_stems',rel=Path(sf));compare('_norm_source_file',p=sf,root=None)
        nodes=[{'id':f'n{i}','label':rng.choice(['Graph Extractor','Parser','User Manager','Build Tool',Path(sf).name]),'file_type':rng.choice(['code','concept','document']),'source_file':sf,'_origin':rng.choice(['ast','semantic']),'source_location':rng.choice(['L1','L2',None])} for i,sf in enumerate(paths)]
        edges=[{'source':f'n{rng.randrange(6)}','target':f'n{rng.randrange(8)}','relation':rng.choice(['calls','imports','references','contains','mentions']),'confidence':rng.choice(['EXTRACTED','INFERRED']),'weight':rng.choice([None,'1.5',-2,0,3.0])} for _ in range(12)]
        extraction={'nodes':nodes,'edges':edges,'hyperedges':[{'id':'he','nodes':['n0',{'id':'n2'},'n7'],'source_file':'docs/guide.md'}]}
        compare('build_from_json',extraction=extraction,directed=rng.choice([False,True]),dedup=rng.choice([True,True,False]))
        compare('_semantic_id_remap',nodes=nodes,root=None);compare('graph_has_legacy_ids',nodes=nodes,root=None)
        # 0.9.80: raw graph endpoints and stub tiers; 0.9.87/0.9.88: lost semantic ids.
        stubs=[{'id':rng.choice(['os','json','gone']),'label':'s','file_type':'concept','type':'external','external':rng.choice([True,True,1]),'source_file':rng.choice(['','x.py']),'_origin':rng.choice(['semantic','ast',None])} for _ in range(3)]
        raw={'nodes':copy.deepcopy(nodes)+stubs,rng.choice(['links','edges']):[dict(e,target=rng.choice([e['target'],'os','N'+e['target'],'missing'])) for e in copy.deepcopy(edges)],'hyperedges':[{'id':'h','nodes':[rng.choice(['gone','n1'])]}]}
        compare('finalize_raw_graph_endpoints',data=raw)
        for stub in stubs:compare('_backfill_origin',item=copy.deepcopy(stub));compare('_is_external_stub',item=stub)
        saved=[dict(n,_origin=rng.choice(['semantic','semantic','ast'])) for n in copy.deepcopy(nodes)]+stubs
        fresh=[dict(n,id=rng.choice([n['id'],'x'+n['id']]),label=rng.choice([n['label'],'Other'])) for n in copy.deepcopy(saved) if rng.random()<0.7]
        compare('_lost_saved_semantic_nodes',saved_nodes=saved,new_nodes=fresh,deleted_sources=set(rng.sample(paths,2)))
    with tempfile.TemporaryDirectory() as temporary:
        root=Path(temporary);gp=root/'graphify-out'/'graph.json';gp.parent.mkdir()
        # Real construction can collapse legacy ghosts in an untouched file.
        # This triggers the loss guard without monkeypatching the native build.
        nodes=[{'id':f'ghost_{i}','label':'same entity','file_type':'concept','source_file':'untouched.md','source_location':f'L{i+1}','_origin':'semantic'} for i in range(2)]
        gp.write_text(json.dumps({'nodes':nodes,'edges':[],'directed':True}))
        compare('build_merge',new_chunks=[],graph_path=gp,dedup=False)
        compare('build_merge',new_chunks=[],graph_path=gp,dedup=True)
        for iteration in range(40):
            existing=[{'id':f'old_{i}','label':f'Old {i}','file_type':'code','source_file':f'file{i%4}.py','_origin':'ast','source_location':f'L{i+1}'} for i in range(12)]
            gp.write_text(json.dumps({'nodes':existing,'links':[{'source':'old_0','target':'old_1','relation':'calls','source_file':'file0.py','_origin':'ast','confidence':'EXTRACTED'}],'directed':iteration%2==0,'hyperedges':[{'id':'h','nodes':['old_6','old_7'],'source_file':'notes.md'}]}))
            fresh={'nodes':[{'id':'fresh','label':'Fresh','file_type':'code','source_file':f'file{iteration%4}.py','_origin':'ast','source_location':'L1'}],'edges':[],'extracted_sources':[f'file{iteration%4}.py']}
            compare('build_merge',new_chunks=[fresh],graph_path=gp,prune_sources=[f'file{(iteration+1)%4}.py'],dedup=iteration%3!=0)
            compare('merge_raw_extraction',new=fresh,graph_path=gp,prune_sources=[f'file{(iteration+1)%4}.py'])
    native.receipt('graphify/build.py',['src/build.rs','src/build_normalize.rs','src/build_merge.rs','src/build_api.rs','src/build_global.rs','src/build_labels.rs','src/hyperedge_members.rs'],original_tests={'files':files,'passed':reports.passed,'skipped':reports.skipped,'deselected':reports.deselected},independent_comparisons=comparisons,complete_graph_comparisons=graph_comparisons,
        adaptations=['Graph construction, replacement, pruning and deduplication execute in Rust. Original export/extract/serve fixtures remain Python callers of this native build API, not proof of a completed application.',
        'Python-only monkeypatch failure injections are replaced by an actual legacy-ghost loss that exercises the native shrink guard with dedup enabled and disabled.',
        'Schema-warning causes and normalized-ID collision ordering are deterministic instead of dependent on Python set hashing.',
        'The JSON binding represents nonfinite numeric test values with their string spellings; the native sanitizer rejects them as graph weights. JSON itself has no NaN/Infinity number tokens.',
        'The original size-cap fixture is exercised through the native GRAPHICLE_MAX_GRAPH_BYTES setting (the reference GRAPHIFY_MAX_GRAPH_BYTES value, copied at the harness boundary), not a Python mocked parser.'])
finally:
    for name,fn in originals.items():setattr(reference,name,fn)
    native.close()
