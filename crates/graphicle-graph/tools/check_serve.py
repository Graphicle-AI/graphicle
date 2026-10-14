"""Compare native graph search/traversal contracts against the pinned module."""
import copy,functools,inspect,json,sys,random
from pathlib import Path
from parity import Native,ROOT,SOURCE
from isolated_upstream import run
import graphify.serve as ref
import networkx as nx
native=Native('serve');counts={};differences=[];inside=False
def graph(g):
    v=nx.node_link_data(g,edges='links')
    # Per-graph caches (0.9.80 caches the traversal view and path-search graphs on G.graph).
    v['graph']={k:x for k,x in v.get('graph',{}).items()if k not in ['_trigram_index','_idf_cache','_traversal_view','_path_search_graphs']}
    return v
def serial(v):
    if isinstance(v,nx.Graph):return serial(graph(v))
    if isinstance(v,Path):return str(v)
    if isinstance(v,dict):return {str(k):serial(x)for k,x in v.items()}
    if isinstance(v,(list,tuple,set,frozenset)):return [serial(x)for x in v]
    return v
def near(a,b):
    if isinstance(a,(int,float))and isinstance(b,(int,float)):return abs(a-b)<=1e-10*max(1,abs(a))
    if isinstance(a,dict)and isinstance(b,dict):return a.keys()==b.keys()and all(near(a[k],b[k])for k in a)
    if isinstance(a,list)and isinstance(b,list):return len(a)==len(b)and all(near(x,y)for x,y in zip(a,b))
    return a==b
def compare(name,request,expected):
    native.use_working_directory(Path.cwd());actual=native.result('serve',**serial(request));counts[name]=counts.get(name,0)+1
    wanted=serial(expected);got=copy.deepcopy(actual)
    if name in ['_bfs','_dfs']:
        wanted=[sorted(wanted[0]),sorted(wanted[1])];got=[sorted(got[0]),sorted(got[1])]
    if name=='_trigrams':wanted=sorted(wanted);got=sorted(got)
    if name=='_query_graph_text':
        # The reference traverses hash sets with per-process randomized order.
        # Preserve node ranking, budget notices and every edge; only compare
        # the order-independent edge block canonically.
        def stable_edges(text):
            lines=text.splitlines();return [s for s in lines if not s.startswith('EDGE ')]+sorted(s for s in lines if s.startswith('EDGE '))
        wanted=stable_edges(wanted);got=stable_edges(got)
    if not near(wanted,got):
        differences.append({'name':name,'request':serial(request),'expected':serial(expected),'actual':actual});(ROOT/'migration/receipts/serve-differences.json').write_text(json.dumps(differences,ensure_ascii=True,indent=2)+'\n');raise AssertionError('Native serve mismatch: '+name)
    return actual
actions={
 '_strip_diacritics':('strip_diacritics',{'text':'value'}), '_search_tokens':('search_tokens',{'text':'value'}),'_trigrams':('trigrams',{'text':'value'}),
 '_node_attributes_text':('attributes_text',{'data':'value'}),'_node_rationale_text':('rationale_text',{'data':'value'}),'_node_search_text':('search_text',{'data':'data','nid':'nid'}),
 '_normalize_context_filters':('normalize_contexts',{'filters':'filters'}),'_infer_context_filters':('infer_contexts',{'question':'question'}),'_resolve_context_filters':('resolve_contexts',{'question':'question','explicit_filters':'filters'}),
 '_compute_idf':('idf',{'G':'graph','terms':'terms'}),'_trigram_candidates':('candidates',{'G':'graph','needles':'needles','guard_frac':'guard_frac'}),
 '_score_nodes':('score_nodes',{'G':'graph','terms':'terms'}),'_score_query':('score_query',{'G':'graph','terms':'terms','collect_per_term_seeds':'collect_per_term_seeds'}),
 '_pick_scored_endpoint':('pick_endpoint',{'G':'graph','scored':'scored','query':'query'}),'_pick_seeds':('pick_seeds',{'G':'graph','scored':'scored','max_k':'max_k','gap_ratio':'gap_ratio','best_seed_by_term':'best_seed_by_term','term_order':'term_order'}),
 '_filter_graph_by_context':('filter_contexts',{'G':'graph','context_filters':'filters'}),'_traversal_view':('traversal_view',{'G':'graph'}),
 '_bfs':('bfs',{'G':'graph','start_nodes':'start_nodes','depth':'depth'}),'_dfs':('dfs',{'G':'graph','start_nodes':'start_nodes','depth':'depth'}),
 '_subgraph_to_text':('subgraph_text',{'G':'graph','nodes':'nodes','edges':'edges','token_budget':'token_budget','seeds':'seeds'}),
 '_cut_lines_to_budget':('cut_lines',{'lines':'lines','token_budget':'token_budget','narrow_hint':'hint'}),'_display_graph_path':('display_path',{'graph_path':'path'}),
 '_query_graph_text':('query_text',{'G':'graph','question':'question','mode':'mode','depth':'depth','token_budget':'token_budget','context_filters':'context_filters','graph_path':'graph_path','seeds':'seeds','seed_root':'seed_root'}),
 '_resolve_path_scoped_symbol':('scoped',{'G':'graph','path_part':'path_part','symbol_part':'symbol_part'}),'_label_has_literal_exact_match':('literal_exact',{'G':'graph','term':'term','norm_query':'norm_query'}),
 '_find_node_tiers':('find_tiers',{'G':'graph','label':'label'}),'_find_node':('find_node',{'G':'graph','label':'label'}),'find_node_ambiguity':('ambiguity',{'G':'graph','label':'label'}),
 '_resolve_single_node':('resolve',{'G':'graph','label':'label'}),'_tier_refusal':('tier_refusal',{'G':'graph','label':'label','tier':'tier','file_query':'file_query'}),
 '_is_ambiguity_refusal':('is_ambiguity_refusal',{'message':'value'}),'_no_scoped_match_message':('no_scoped_match',{'G':'graph','path_part':'path_part','symbol_part':'symbol_part'}),
 '_resolve_python_qualified':('python_qualified',{'G':'graph','label':'label'}),'_shortest_path_text':('shortest_path_text',{'G':'graph','arguments':'arguments'}),'_community_header':('community_header',{'cid':'cid','community_name':'name'}),
}
for name,(action,fields)in actions.items():
    original=getattr(ref,name);sig=inspect.signature(original)
    @functools.wraps(original)
    def wrapper(*args,_name=name,_action=action,_fields=fields,_original=original,_sig=sig,**kw):
        global inside
        if inside:return _original(*args,**kw)
        a=_sig.bind(*args,**kw);a.apply_defaults();req={'action':_action,**{dest:a.arguments[src]for src,dest in _fields.items()}}
        G=a.arguments.get('G')
        if G is not None:
            req['neighbor_order']=[[n,list(G.neighbors(n))]for n in G.nodes]
            if G.is_directed():req['predecessor_order']=[[n,list(G.predecessors(n))]for n in G.nodes]
        if _name=='_node_search_text':
            g=nx.Graph();g.add_node(a.arguments['nid'],**a.arguments['data']);req['graph']=g;req.pop('data')
        if _name=='_pick_seeds'and G is None:
            g=nx.Graph();g.add_nodes_from(n for _,n in a.arguments['scored']);req['graph']=g;req['without_graph']=True
        inside=True
        try:expected=_original(*args,**kw)
        finally:inside=False
        if _name=='_score_query':expected={'ranked':expected.ranked,'best_seed_by_term':expected.best_seed_by_term,'term_order':list(expected.term_order)}
        actual=compare(_name,req,expected)
        if _name=='_score_query':return ref._QueryScores([tuple(x)for x in actual['ranked']],actual['best_seed_by_term'],tuple(actual['term_order']))
        if _name=='_score_nodes':return [tuple(x)for x in actual]
        if _name in ['_bfs','_dfs']:return set(actual[0]),[tuple(e)for e in actual[1]]
        if _name=='_trigrams':return set(actual)
        if _name in ['_node_attributes_text','_resolve_context_filters','_find_node_tiers','_resolve_single_node']:return tuple(actual)
        # Equal by the comparison above; the reference object keeps per-graph
        # cache identity (0.9.80 `_traversal_view` is built once per graph).
        if isinstance(expected,nx.Graph):return expected
        return actual
    setattr(ref,name,wrapper)
if __name__=='__main__':
    paths=['tests/'+p.name for p in sorted((SOURCE/'tests').glob('test_serve*.py'))if 'http'not in p.name]
    paths += ['tests/test_query_mcp_direction.py','tests/test_query_induced_edges.py','tests/test_file_label_disambiguation.py','tests/test_terraform.py','tests/test_query_names_its_graph.py']
    # 0.9.84-0.9.88: lookup refusals and exit codes, same-file ties, budget hints, seeds, cached views.
    paths += ['tests/test_lookup_exit_codes.py','tests/test_explain_same_file_tie.py','tests/test_truncation_full_answer.py','tests/test_query_seed_order.py','tests/test_query_seeds.py','tests/test_path_cli.py','tests/test_mcp_graph_view_cache.py']
    # 0.9.84-0.9.88: `explain` through the CLI (tiers, refusals, --all).
    paths += ['tests/test_explain_cli.py']
    if '--test'in sys.argv:paths=[sys.argv[sys.argv.index('--test')+1]]
    code,tests=run(paths);assert code==0 and not differences
    rng=random.Random(503)
    tied=nx.DiGraph()
    tied.add_nodes_from((s,{'label':s})for s in ['s','a','b','c','y','z','t'])
    tied.add_edges_from([('s','a'),('s','b'),('s','c'),('a','z'),('z','t'),('b','y'),('y','t')])
    ref._shortest_path_text(tied,{'source':'s','target':'t'})
    for case in range(600):
        g=(nx.MultiDiGraph() if case%3==0 else nx.DiGraph() if case%3==1 else nx.Graph())
        ids=[f'n{i:02}'for i in range(rng.randrange(3,18))];rng.shuffle(ids)
        for nid in ids:g.add_node(nid,label='Symbol'+nid,source_file=nid+'.rs')
        for _ in range(rng.randrange(len(ids),len(ids)*3)):
            u,v=rng.sample(ids,2);src,tgt=(v,u)if rng.randrange(4)==0 else(u,v)
            g.add_edge(u,v,relation=rng.choice(['calls','references','contains']),confidence=rng.choice(['EXTRACTED','INFERRED']),_src=src,_tgt=tgt)
        src,tgt=rng.sample(ids,2)
        ref._shortest_path_text(g,{'source':src,'target':tgt,'undirected':bool(case%2),'max_hops':rng.choice([1,2,8])})
    native.close()
    not_run={'tests/test_serve_http.py':'Module-level importorskip("mcp") and ("starlette"): the pinned reference venv (tools/reference-requirements.txt) has neither, so the file skips on '
             'both sides, and its Starlette TestClient drives the reference ASGI app in process, which has no native seam. The native HTTP transport is driven by '
             'check_native_workflows (MCP phase); its new graph_stats build-commit cases (#3354) are translated in tests/compat_0_9_88_cli.rs (graph_stats_name_the_build_commit_and_staleness).'}
    native.receipt('graphify/serve.py',[str(p.relative_to(ROOT))for p in sorted((ROOT/'src').glob('serve*.rs'))]+['src/assets/serve/constants.json'],checked=counts,upstream_tests=tests,upstream_tests_not_run=not_run,interface_differences=['Traversal uses stable native sets; edge presentation order does not depend on Python hash randomization.'])
