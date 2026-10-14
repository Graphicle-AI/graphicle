"""Original rebuild contracts exercised through the native reconciliation layer."""
import copy, functools, inspect, json, sys
from pathlib import Path
from parity import Native, ROOT, SOURCE
from isolated_upstream import run
import naming
import graphify.watch as ref
from graphify.build import _norm_source_file
native=Native('watch');counts={};differences=[]
def serial(v):
    if isinstance(v,Path):return str(v)
    if isinstance(v,dict):return {str(k):serial(x)for k,x in v.items()}
    if isinstance(v,(list,tuple,set,frozenset)):return [serial(x)for x in v]
    return v
def canon(v):
    v=serial(v)
    if isinstance(v,dict):
        v={k:canon(x)for k,x in v.items()}
        for k in ['nodes','edges','links','hyperedges','deleted_paths']:
            if isinstance(v.get(k),list):v[k].sort(key=lambda x:json.dumps(x,sort_keys=True))
    return v
def markers(request):
    """The project-root marker the reference orchestration wrote, for the native call under its name."""
    folders=[Path(v) for k in ('path','watch_root','change_root','project_root','out') for v in [request.get(k)] if isinstance(v,(str,Path)) and str(v)]
    return naming.native_copies(*[f/sub/'.graphify_root' for f in folders for sub in ('.',naming.UPSTREAM_OUT)])
# Native actions whose JSON reply wraps the compared value (the shrink guard
# also returns the warnings it would print).
VALUE_REPLIES={'_check_shrink'}
def compare(name,request,expected):
    native.use_working_directory(Path.cwd())
    with markers(request):actual=native.result('watch',**serial(request))
    if name in VALUE_REPLIES:actual=actual['value']
    counts[name]=counts.get(name,0)+1
    if canon(expected)!=canon(actual):
        differences.append({'name':name,'request':serial(request),'expected':serial(expected),'actual':actual})
        (ROOT/'migration/receipts/watch-differences.json').write_text(json.dumps(differences,indent=2)+'\n')
        raise AssertionError('Native watch mismatch: '+name)
    return actual
simple={
 'check_update':lambda a:{'action':'check_update','path':a['watch_path']},
 '_report_root_label':lambda a:{'action':'report_root','path':a['watch_path']},
 '_changed_path_candidates':lambda a:{'action':'candidates','path':a['raw'],'change_root':a['change_root'],'watch_root':a['watch_root']},
 '_is_remote_source':lambda a:{'action':'remote','source':a['source_file']},
 '_node_community_map':lambda a:{'action':'node_communities','data':a['graph_data']},
 '_canonical_graph_for_compare':lambda a:{'action':'canonical','data':a['graph_data']},
 '_canonical_topology_for_compare':lambda a:{'action':'canonical','data':a['graph_data'],'topology':True},
 '_report_for_compare':lambda a:{'action':'report_compare','text':a['report_text']},
 '_has_non_code':lambda a:{'action':'has_non_code','paths':a['changed_paths']},
 '_batch_triggers_rebuild':lambda a:{'action':'triggers','paths':a['batch']},
 '_batch_needs_llm_flag':lambda a:{'action':'needs_llm','paths':a['batch']},
 '_check_shrink':lambda a:{'action':'check_shrink','force':a['force'],'existing':a['existing_data'],'new':a['new_data'],'had_explicit_deletions':a['had_explicit_deletions'],'rebuilt_sources':a['rebuilt_sources'],'failed_sources':a['failed_sources'] or [],'deleted_sources':sorted(a.get('deleted_sources') or [])},
}
for name,request in simple.items():
    original=getattr(ref,name);sig=inspect.signature(original)
    @functools.wraps(original)
    def wrapper(*args,_original=original,_sig=sig,_request=request,_name=name,**kw):
        a=_sig.bind(*args,**kw);a.apply_defaults();expected=_original(*args,**kw);compare(_name,_request(a.arguments),expected);return expected
    setattr(ref,name,wrapper)
for name in ['_relativize_source_files','_rebase_relative_source_files']:
    original=getattr(ref,name);sig=inspect.signature(original)
    @functools.wraps(original)
    def wrapper(*args,_original=original,_sig=sig,_name=name,**kw):
        a=_sig.bind(*args,**kw);a.apply_defaults();a=a.arguments;before=copy.deepcopy(a['payload']);_original(*args,**kw)
        req={'action':'relativize','data':before,'root':a['root'],'scope':a['scope']}if _name=='_relativize_source_files'else{'action':'rebase','data':before,'source_root':a['source_root'],'target_root':a['target_root']}
        actual=compare(_name,req,a['payload']);a['payload'].clear();a['payload'].update(actual)
    setattr(ref,name,wrapper)
original_reconcile=ref._reconcile_existing_graph
@functools.wraps(original_reconcile)
def reconcile(existing_graph,result,**kw):
    before=copy.deepcopy(result);deleted=set(kw['deleted_paths']);existing=json.loads(existing_graph.read_text())if existing_graph.exists()else None
    expected,data=original_reconcile(existing_graph,result,**kw)
    if existing is None:return expected,data
    sp=ref._StoredSourcePaths(existing,out=kw['out'],project_root=kw['project_root'],watch_root=kw['watch_root'],normalize_source=_norm_source_file)
    req={'action':'reconcile','existing':existing,'result':before,**{k:v for k,v in kw.items()if not k.startswith('is_ignored_')},'deleted_paths':deleted}
    for k in ['always','full']:
        f=kw.get('is_ignored_'+k);req['ignored_'+k]=[sp.identity(n.get('source_file'))for n in existing.get('nodes',[])if n.get('source_file') and f and f(Path(sp.identity(n['source_file'])))]
    native.use_working_directory(Path.cwd())
    with markers(req):actual=native.result('watch',**serial(req))
    counts['reconcile']=counts.get('reconcile',0)+1
    wanted={'result':expected,'deleted_paths':sorted(kw['deleted_paths'])};got={k:actual[k]for k in wanted}
    if canon(wanted)!=canon(got):
        differences.append({'name':'reconcile','request':serial(req),'expected':serial(wanted),'actual':got});(ROOT/'migration/receipts/watch-differences.json').write_text(json.dumps(differences,indent=2)+'\n');raise AssertionError('Native watch reconcile differs')
    return actual['result'],data
ref._reconcile_existing_graph=reconcile
# 0.9.80-0.9.88 build contracts (#3413 compact graph.json, run-only keys, raw
# endpoints, graph invariance) are end-to-end: the whole rebuild and the CLI
# children run natively. Other functions these files call directly (to_json,
# extract, finalize_raw_graph_endpoints, prune, the global graph) run the
# reference here and natively in check_export, check_extract, check_build,
# check_cli and check_global_graph.
END_TO_END=['tests/test_compact_graph_json.py','tests/test_no_cluster_run_only_keys.py','tests/test_no_cluster_undeclared_endpoints.py','tests/test_graph_invariance.py']
original_rebuild=ref._rebuild_code
@functools.wraps(original_rebuild)
def native_rebuild(watch_path,*,changed_paths=None,follow_symlinks=False,force=False,no_cluster=False,acquire_lock=True,block_on_lock=False):
    native.use_working_directory(Path.cwd())
    result=native.result('watch',action='rebuild',path=str(watch_path),changed_paths=None if changed_paths is None else [str(p) for p in changed_paths],
                         follow_symlinks=follow_symlinks,force=force,no_cluster=no_cluster,acquire_lock=acquire_lock,block_on_lock=block_on_lock)
    for message in result['messages']:print(naming.to_upstream(message,'upstream'))
    counts['_rebuild_code']=counts.get('_rebuild_code',0)+1
    return result['success']
if __name__=='__main__':
    from upstream_cli import native_graphify
    paths=['tests/'+p.name for p in sorted((SOURCE/'tests').glob('test_watch*.py'))]
    # 0.9.78 (#4161): never-scanned placeholders survive a reconcile; 0.9.80
    # (#4189): reconciled external stubs and raw endpoints.
    paths+=['tests/test_incremental_unscanned_placeholders.py','tests/test_external_stub_endpoints.py']
    end_to_end=[p for p in END_TO_END if (SOURCE/p).is_file()]
    if '--test' in sys.argv:
        chosen=sys.argv[sys.argv.index('--test')+1]
        paths,end_to_end=([],[chosen]) if chosen.split('::')[0] in END_TO_END else ([chosen],[])
    code,tests=run(paths) if paths else (0,{'paths':[],'passed':0,'skipped':0,'not_applicable':[]})
    assert code==0 and not differences,code
    cli_runs={}
    if end_to_end:
        ref._rebuild_code=native_rebuild
        try:
            with native_graphify(cli_runs):code,end_to_end_tests=run(end_to_end)
        finally:ref._rebuild_code=original_rebuild
        assert code==0 and not differences,code
        tests={'reconciliation':tests,'end_to_end':{**end_to_end_tests,'native_rebuilds':counts.get('_rebuild_code',0),'native_cli_runs':cli_runs}}
    native.close()
    native.receipt('graphify/watch.py',['src/watch.rs','src/watch_paths.rs','src/watch_reconcile.rs'],checked=counts,upstream_tests=tests,interface_differences=['Native polling observer on all platforms; persistent inode locks avoid unlink races.','End-to-end files (end_to_end): _rebuild_code is the native watch rebuild and every `python -m graphify` child runs the native executable (tools/upstream_cli.py native_graphify, layout upstream).'])
