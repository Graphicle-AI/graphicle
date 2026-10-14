#!/usr/bin/env python3
"""Original HTML checks plus exact documents from matched graph inputs."""
import contextlib,inspect,io,json,os,random,sys,tempfile,types
from pathlib import Path
from parity import Native,SOURCE
from naming import to_upstream
import upstream_naming
import networkx as nx
import pytest
import graphify.export as export
import graphify.exporters.html as reference

oracle=types.ModuleType('reference_export_html');oracle.__package__='graphify.exporters'
exec(compile((SOURCE/'graphify/exporters/html.py').read_text(),str(SOURCE/'graphify/exporters/html.py'),'exec'),oracle.__dict__)
native=Native('export_html');saved={};env=None;documents=0
def plain(v):
    if isinstance(v,nx.Graph):return nx.node_link_data(v,edges='edges')
    if isinstance(v,Path):return str(v)
    return v
def call(fn,**kw):
    global env
    state=tuple(os.environ.get(k) for k in ('GRAPHIFY_VIZ_NODE_LIMIT','GRAPHIFY_OUT'))
    if state!=env:native.cwd=None;env=state
    native.use_working_directory(os.getcwd())
    answer=to_upstream(native.call('exporters.html',function=fn,**{k:plain(v) for k,v in kw.items()}))
    if not answer['ok']:raise ValueError(answer['error'])
    for s in answer['result']['stdout']:print(s)
    for s in answer['result']['stderr']:print(s,file=sys.stderr)
    return answer['result']['value']
def wrap(fn):
    sig=inspect.signature(getattr(reference,fn))
    def invoke(*args,**kwargs):
        bound=sig.bind(*args,**kwargs);bound.apply_defaults();return call(fn,**bound.arguments)
    return invoke
class Reports:
    passed=0;skipped=0
    def pytest_collection_modifyitems(self,config,items):
        # test_export.py: the HTML and hyperedge cases. The 0.9.85/0.9.87 files
        # (#4124 script breakout, #3894 theme) render graph.html only; the
        # breakout file's tree.html case belongs to check_tree_html.py.
        def keep(i):
            file=Path(str(i.path)).name
            if file=='test_html_theme.py':return True
            if file=='test_html_script_breakout.py':return 'tree_html' not in i.originalname
            return i.originalname.startswith(('test_to_html','test_html_document_title','test_hyperedge_perimeter','test_hyperedge_convex_hull'))
        chosen=[i for i in items if keep(i)]
        deselected=[i for i in items if i not in chosen];items[:]=chosen;config.hook.pytest_deselected(items=deselected)
    def pytest_runtest_logreport(self,report):
        if report.when=='call' and report.passed:self.passed+=1
        if report.skipped:self.skipped+=1
reports=Reports()
try:
    for name in ('_viz_node_limit','_html_styles','_hyperedge_script','_html_script','_html_document_title','to_html'):
        saved[name]=getattr(reference,name);setattr(reference,name,wrap(name))
    old_export=export.to_html;export.to_html=reference.to_html
    files=['tests/test_export.py','tests/test_html_theme.py','tests/test_html_script_breakout.py']
    with upstream_naming.isolated(files,'check_export_html') as (_,copies):status=pytest.main(['-q','-p','no:cacheprovider',*copies],plugins=[reports])
    assert status==0,status
    for name,value in saved.items():setattr(reference,name,value)
    export.to_html=old_export
    for raw in ['','0','-1','+23','  1024 ','bad','1_000','1__0','١٢٣','９９','9999999999999999999999999']:
        os.environ['GRAPHIFY_VIZ_NODE_LIMIT']=raw
        assert call('_viz_node_limit')==oracle._viz_node_limit(),raw
    os.environ.pop('GRAPHIFY_VIZ_NODE_LIMIT',None)
    rng=random.Random(67)
    with tempfile.TemporaryDirectory() as temporary:
        out=Path(temporary)/'graphify-out'/'graph.html';out.parent.mkdir()
        for case in range(160):
            G=rng.choice([nx.Graph,nx.DiGraph,nx.MultiGraph,nx.MultiDiGraph])()
            n=rng.randrange(0,30)
            for i in range(n):G.add_node(f'n{i}',label=rng.choice(['a&b','日本語','{{EDGES}}','<script></script>','Orders',None]),source_file=rng.choice(['src/main.py',None,'lib/API.ts']),file_type='code')
            for _ in range(n*2):
                u=f'n{rng.randrange(n)}';v=f'n{rng.randrange(n)}'
                attrs={'relation':rng.choice(['calls','has </script>','uses']),'confidence':rng.choice(['EXTRACTED','INFERRED','AGGREGATED'])}
                if rng.random()<0.2:attrs.update(_src=v,_tgt=u)
                G.add_edge(u,v,**attrs)
            communities={c:[f'n{i}' for i in range(n) if i%3==c] for c in range(3)}
            labels={0:'Orders',1:'<x> & "Y"',2:'日本語'}
            G.graph['hyperedges']=[{'id':'h','label':'Constraint','nodes':[f'n{i}' for i in range(min(n,5))]}]
            overlay={f'n{i}':{'status':rng.choice(['preferred','contested','tentative']),'stale':rng.choice([True,False]),'uses':rng.randrange(10),'neg':2,'score':0.7} for i in range(n) if i%4==0}
            kwargs=dict(G=G,communities=communities,output_path=str(out),community_labels=labels,learning_overlay=overlay)
            if case%4==0:kwargs['node_limit']=5
            elif case%4==1:kwargs['member_counts']={0:5,1:3,2:1}
            with contextlib.redirect_stdout(io.StringIO()):expected=oracle.to_html(**kwargs)
            before=out.read_text();out.unlink()
            with contextlib.redirect_stdout(io.StringIO()):actual=call('to_html',**kwargs)
            after=to_upstream(out.read_text())
            if expected!=actual or before!=after:native.fail(case=case,expected=before,actual=after,graph=plain(G),kwargs={k:plain(v) for k,v in kwargs.items() if k!='G'})
            documents+=1
        # Single-community aggregation is intentionally skipped; default cap is an error.
        G=nx.path_graph([f'x{i}' for i in range(6)])
        assert call('to_html',G=G,communities={0:list(G)},output_path=out,node_limit=2) is False
        os.environ['GRAPHIFY_VIZ_NODE_LIMIT']='2'
        try:call('to_html',G=G,communities={0:list(G)},output_path=out)
        except ValueError as e:assert 'too large' in str(e)
        else:native.fail(error='size cap did not reject graph')
        os.environ.pop('GRAPHIFY_VIZ_NODE_LIMIT',None)
    native.receipt('graphify/exporters/html.py',['src/exporters/html.rs','src/exporters/common.rs','src/assets/export_html/styles.html','src/assets/export_html/script.html.template','src/assets/export_html/hyperedges.html.template','src/assets/export_html/document.html.template'],original_tests={'files':files,'selected':'HTML and hyperedge rendering, script breakout (graph.html), theme toggle','passed':reports.passed,'skipped':reports.skipped},exact_documents=documents,adaptations=['The existing browser JavaScript/CSS/HTML literals retain their format. All Python data assembly, aggregation, learned sidecar loading and atomic output are native Rust.','The existing JavaScript geometry test runs the emitted native asset in Node.js, without rewriting the original test.'])
finally:
    for name,value in saved.items():setattr(reference,name,value)
    if 'old_export' in globals():export.to_html=old_export
    native.close()
