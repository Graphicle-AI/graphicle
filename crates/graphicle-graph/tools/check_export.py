#!/usr/bin/env python3
"""Qualify native exports using retained Graphify cases and complete artifacts."""
import contextlib,copy,inspect,io,json,os,random,sys,tempfile,types
from pathlib import Path
from parity import Native,SOURCE
from naming import NATIVE_FIELDS,NATIVE_FUNCTIONS,to_upstream
import upstream_naming
import networkx as nx
import numpy as np
import pytest
import graphify.export as reference
import graphify.security as security

oracle=types.ModuleType('reference_export');oracle.__package__='graphify'
exec(compile((SOURCE/'graphify/export.py').read_text(),str(SOURCE/'graphify/export.py'),'exec'),oracle.__dict__)
native=Native('export');saved={};environment=None;helpers=0;artifacts=0;layouts=0;energy_checks=[]
def plain(v):
    if isinstance(v,nx.Graph):return nx.node_link_data(v,edges='edges')
    if isinstance(v,Path):return str(v)
    if isinstance(v,(set,tuple)):return list(v)
    return v
def call(function,**kw):
    global environment
    settings=(os.environ.get('GRAPHIFY_NO_BACKUP'),os.environ.get('GRAPHIFY_MAX_GRAPH_BYTES'),os.environ.get('GRAPHIFY_VIZ_NODE_LIMIT'))
    if settings!=environment:native.cwd=None;environment=settings
    native.use_working_directory(os.getcwd())
    if isinstance(kw.get('G'),nx.Graph):kw['neighbor_order']=[[n,list(kw['G'][n])] for n in kw['G']]
    r=to_upstream(native.call('export',function=NATIVE_FUNCTIONS.get(function,function),**{k:plain(v) for k,v in kw.items()}))
    if not r['ok']:raise ValueError(r['error'])
    for line in r['result']['stdout']:print(line)
    for line in r['result']['stderr']:print(line,file=sys.stderr)
    return r['result']['value']
def wrap(name):
    sig=inspect.signature(getattr(reference,name))
    def invoke(*args,**kwargs):
        bound=sig.bind(*args,**kwargs);bound.apply_defaults();kw=dict(bound.arguments)
        if name=='to_obsidian':
            out=kw['output_dir'];kw['stem_limit']=reference.stem_filename_budget(out,reserve=5);kw['community_stem_limit']=reference.stem_filename_budget(out,reserve=15)
        if name=='to_canvas':kw['stem_limit']=reference.stem_filename_budget(Path(kw['output_path']).parent,reserve=5)
        result=call(name,**kw)
        if name=='attach_hyperedges':kw['G'].graph.clear();kw['G'].graph.update(result['graph']);return None
        if name=='backup_if_protected':return Path(result) if result is not None else None
        if name=='existing_graph_node_count':return reference.MALFORMED_GRAPH if result=={'malformed':True} else result
        if name=='_adopt_pre_manifest_notes':return set(result)
        if name=='prune_dangling_edges':kw['graph_data'].clear();kw['graph_data'].update(result[0]);return kw['graph_data'],result[1]
        return result
    return invoke
class Reports:
    passed=0;skipped=0;deselected=[]
    def pytest_collection_modifyitems(self,config,items):
        chosen=[];omitted=[]
        for item in items:
            name=item.originalname;file=Path(str(item.path)).name
            keep=True
            if file=='test_export.py':keep=not name.startswith(('test_to_html','test_html_document_title','test_hyperedge_perimeter','test_hyperedge_convex_hull'))
            if file=='test_hypergraph.py':keep=name.startswith(('test_attach_hyperedges','test_to_json','test_hyperedges_roundtrip'))
            if file=='test_export_path_length.py':keep=name.startswith(('test_obsidian','test_canvas','test_community_'))
            if file=='test_confidence.py':keep=name in ('test_confidence_score_round_trip','test_to_json_defaults_missing_confidence_score')
            # 0.9.86 (#3413): the other cases run end to end in check_watch.py.
            if file=='test_compact_graph_json.py':keep=name=='test_to_json_writes_one_compact_line'
            (chosen if keep else omitted).append(item)
        items[:]=chosen;self.deselected=[i.nodeid for i in omitted];config.hook.pytest_deselected(items=omitted)
    def pytest_runtest_logreport(self,report):
        if report.when=='call' and report.passed:self.passed+=1
        if report.skipped:self.skipped+=1
reports=Reports()
def compare(function,**kw):
    global helpers
    expected=getattr(oracle,function)(**kw);actual=call(function,**kw);helpers+=1
    if plain(expected)!=actual:native.fail(function=function,arguments=kw,expected=plain(expected),actual=actual)
def tree(path):return {str(p.relative_to(path)):p.read_text() for p in path.rglob('*') if p.is_file()}
def writer_version(text,reference):
    """graph.json records the release that wrote it (#4167): the reference its
    graphifyy version under graph.graphify_version, the engine its own under
    graph.graphicle_version (naming.NATIVE_FIELDS). Only that key and value
    are replaced before the byte comparison."""
    if not text.startswith('{'):return text
    key=NATIVE_FIELDS['graphify_version']
    native_version=json.loads(text)['graph'].get(key)
    if native_version is None:return text
    expected=json.loads(reference)['graph'].get('graphify_version')
    return text.replace(json.dumps(key)+':'+json.dumps(native_version),'"graphify_version":'+json.dumps(expected),1)
try:
    for name in ('backup_if_protected','_obsidian_tag','_strip_diacritics','_yaml_str','attach_hyperedges','_git_head','existing_graph_node_count','to_json','prune_dangling_edges','_cypher_escape','_cypher_label','to_cypher','_strip_xml_illegal','_cap_filename','_is_graphify_note','_adopt_pre_manifest_notes','_obsidian_safe_stem','to_obsidian','to_canvas','to_graphml','to_svg'):
        saved[name]=getattr(reference,name);setattr(reference,name,wrap(name))
    files=['tests/test_export.py','tests/test_export_control_characters.py','tests/test_export_path_length.py','tests/test_obsidian_unicode_tags.py','tests/test_obsidian_vault_migration.py','tests/test_hyperedge_roundtrip.py','tests/test_hypergraph.py','tests/test_confidence.py','tests/test_compact_graph_json.py',
        # First run here on 14 October 2026. Of test_community_labels_skill.py
        # only the to_json cases: the others read Graphify's own skill text.
        'tests/test_obsidian_dangling_member.py','tests/test_obsidian_filename_cap.py',
        'tests/test_community_labels_skill.py::test_to_json_community_labels_kwarg_writes_community_name',
        'tests/test_community_labels_skill.py::test_to_json_without_labels_kwarg_writes_no_community_name',
        'tests/test_export_idempotent_writes.py::test_to_obsidian_rerun_does_not_rewrite_unchanged_notes',
        'tests/test_export_idempotent_writes.py::test_to_obsidian_rerun_still_rewrites_changed_notes',
        # 0.9.95 (#4709): tag values cannot break out of the frontmatter list.
        'tests/test_frontmatter_and_ssrf_hardening.py::test_obsidian_tag_strips_yaml_breaking_chars']
    with upstream_naming.isolated(files,'check_export') as (_,copies):status=pytest.main(['-q','-p','no:cacheprovider',*copies],plugins=[reports])
    assert status==0,status
    for name,value in saved.items():setattr(reference,name,value)
    rng=random.Random(70)
    strings=['','...','a\x00\x7fb','東京','𝟙²③','ééé','a'+chr(0x301),'CLAUDE.md','{{WORDS}}']
    alphabet='abcXYZ_-/😀日本語é\n\t\r'+''.join(chr(i) for i in range(32))
    strings += [''.join(rng.choice(alphabet) for _ in range(rng.randrange(300))) for _ in range(100)]
    for s in strings:
        compare('_obsidian_tag',name=s);compare('_strip_diacritics',text=s);compare('_yaml_str',s=s);compare('_cypher_escape',s=s);compare('_strip_xml_illegal',s=s)
        for limit in (5,40,200):compare('_cap_filename',s=s,limit=limit);compare('_obsidian_safe_stem',label=s,limit=limit)
    with tempfile.TemporaryDirectory() as temporary:
        root=Path(temporary);a=root/'a';b=root/'b';a.mkdir();b.mkdir()
        # 0.9.95 (#4709): a hostile file_type or edge confidence reaches the note's tags sanitized.
        H=nx.Graph();H.add_node('n0',label='A',file_type='document\ninjected_key: pwned',source_file='a.md');H.add_node('n1',label='B',file_type='code #x',source_file='b.py');H.add_edge('n0','n1',relation='calls',confidence='INFERRED\nevil: x')
        kwargs={'G':H,'communities':{0:['n0','n1']},'community_labels':{0:'C'},'cohesion':{0:0.5}}
        with contextlib.redirect_stderr(io.StringIO()):expected=oracle.to_obsidian(**kwargs,output_dir=str(root/'hostile-a'));actual=call('to_obsidian',**kwargs,output_dir=root/'hostile-b')
        if expected!=actual or tree(root/'hostile-a')!=to_upstream(tree(root/'hostile-b')):native.fail(function='to_obsidian',expected=tree(root/'hostile-a'),actual=tree(root/'hostile-b'))
        artifacts+=1
        for iteration in range(45):
            G=rng.choice([nx.Graph,nx.DiGraph])()
            for i in range(rng.randrange(1,15)):G.add_node(f'n{i}',label=rng.choice(['API','Api','index.ts','index.ts_1','日本語','é & "words"'])+str(i%3),file_type=rng.choice(['code','document','']),source_file=f'src/{i}.py',count=i,truth=bool(i%2),metadata={'k':[i]},_origin='ast')
            for _ in range(len(G)*2):G.add_edge(rng.choice(list(G)),rng.choice(list(G)),relation=rng.choice(['calls','contains','references']),confidence=rng.choice(['INFERRED','EXTRACTED','AMBIGUOUS']),weight=rng.choice([0.5,1.0,3.0]))
            # Stored direction as load_node_link_graph stamps it, sometimes against arc order.
            for u,v,data in G.edges(data=True):
                if rng.random()<0.4:data['_src'],data['_tgt']=(v,u) if rng.random()<0.5 else (u,v)
            communities={c:[f'n{i}' for i in range(len(G)) if i%3==c] for c in range(3)};labels={0:'API',1:'Api',2:'記憶'}
            G.graph['hyperedges']=[{'id':'h1','nodes':list(G)[:3],'label':'all required'}]
            for name,suffix,extra in [('to_json','json',{'force':True,'built_at_commit':'abc','community_labels':labels}),('to_cypher','cypher',{}),('to_canvas','canvas',{'community_labels':labels})]:
                pa=a/f'g.{suffix}';pb=b/f'g.{suffix}'
                kw={'G':G,'communities':communities} if name!='to_cypher' else {'G':G}
                expected=getattr(oracle,name)(**kw,output_path=str(pa),**extra);actual=call(name,**kw,output_path=pb,**extra)
                written=to_upstream(pb.read_bytes().decode('utf-8'))
                if name=='to_json':written=writer_version(written,pa.read_text())
                if expected!=actual or pa.read_bytes()!=written.encode('utf-8'):native.fail(function=name,expected=pa.read_text(),actual=pb.read_text())
                artifacts+=1
            kwargs={'G':G,'communities':communities,'community_labels':labels,'cohesion':{0:0.25,1:0.75,2:0.9}}
            with contextlib.redirect_stderr(io.StringIO()):expected=oracle.to_obsidian(**kwargs,output_dir=str(a/'vault'));actual=call('to_obsidian',**kwargs,output_dir=b/'vault')
            if expected!=actual or tree(a/'vault')!=to_upstream(tree(b/'vault')):native.fail(function='to_obsidian',expected=tree(a/'vault'),actual=tree(b/'vault'))
            artifacts+=1
            pa=a/'g.graphml';pb=b/'g.graphml';oracle.to_graphml(G,communities,str(pa));call('to_graphml',G=G,communities=communities,output_path=pb)
            ga=nx.node_link_data(nx.read_graphml(pa),edges='edges');gb=nx.node_link_data(nx.read_graphml(pb),edges='edges')
            if ga!=gb:native.fail(function='to_graphml',expected=ga,actual=gb)
            artifacts+=1
            layout=nx.spring_layout(G,seed=42,k=2.0/(len(G)**0.5+1));original=np.array([layout[n] for n in G]);actual=np.array(call('spring_layout',G=G))
            if not np.allclose(original,actual,rtol=0,atol=1e-8):native.fail(function='spring_layout',expected=original.tolist(),actual=actual.tolist())
            layouts+=1
        import xml.etree.ElementTree as ET
        call('to_svg',G=G,communities=communities,output_path=root/'g.svg',community_labels=labels)
        svg=ET.parse(root/'g.svg');ns={'s':'http://www.w3.org/2000/svg'}
        assert len(svg.findall('.//s:circle',ns))==len(G)
        assert len(svg.findall('.//s:line',ns))==G.number_of_edges()
        for directed in (False,True):
            G=nx.gnp_random_graph(501,0.008,seed=70,directed=directed)
            for u,v,d in G.edges(data=True):d['weight']=float(1 if (u+v)%2 else -2)
            A=np.abs(nx.to_numpy_array(G,dtype=np.float32));A=(A+A.T)/2
            k=2/(len(G)**0.5+1)
            components=list(nx.connected_components(nx.from_numpy_array(A)))
            def energy(pos):
                delta=pos[:,None,:]-pos[None,:,:];d2=np.maximum((delta*delta).sum(axis=-1),1e-10);d=np.sqrt(d2)
                value=np.sum(A*d2*d)/(3*k)-k*k*np.log(d).sum()
                for members in components:
                    center=pos[list(members)].mean(axis=0)-0.5;value+=0.5*len(members)*np.dot(center,center)
                return float(value)
            initial=nx.rescale_layout(np.random.RandomState(42).rand(len(G),2).astype(np.float32).astype(float))
            actual=np.array(call('spring_layout',G=G));expected_layout=nx.spring_layout(G,seed=42,k=k)
            expected=np.array([expected_layout[n] for n in G])
            assert np.isfinite(actual).all() and np.abs(actual).max()<=1.00000001
            before=energy(initial);after=energy(actual);reference_cost=energy(expected)
            assert after<before, (before,after)
            # This compares objective quality, not optimizer iterations or positions.
            assert after<=reference_cost*1.25,(after,reference_cost)
            energy_checks.append({'nodes':len(G),'directed':directed,'initial_rescaled_cost':before,'rust_rescaled_cost':after,'networkx_rescaled_cost':reference_cost,'same_positions_required':False})
        G=nx.Graph()
        for n,label in [('parse','Parse sources'),('symbols','Resolve symbols'),('graph','Build graph'),('query','Find evidence'),('answer','Cite sources'),('vault','Export notes')]:G.add_node(n,label=label)
        G.add_edges_from([('parse','symbols'),('symbols','graph'),('graph','query'),('query','answer'),('graph','vault')])
        G.edges['graph','query']['confidence']='INFERRED'
        call('to_svg',G=G,communities={0:['parse','symbols','graph'],1:['query','answer','vault']},output_path=Path('migration/receipts/export-preview.svg').resolve(),community_labels={0:'Code analysis',1:'Evidence and exports'},figsize=[12,7])
    native.receipt('graphify/export.py',['src/export.rs','src/export_obsidian.rs','src/export_formats.rs','src/export_api.rs','src/assets/export/digit_ranges.json'],original_tests={'files':files,'passed':reports.passed,'skipped':reports.skipped,'deselected':reports.deselected},helper_comparisons=helpers,complete_artifacts=artifacts,force_layout_comparisons=layouts,energy_layout_checks=energy_checks,interface_differences=['graph.json records the release that wrote it (Graphify 0.9.78, #4167): the reference its graphifyy version under graph.graphify_version, the engine its own (<ported Graphify release>-graphicle.<engine version>) under graph.graphicle_version (naming.NATIVE_FIELDS). The complete-file comparison maps only that key and value; test_export.py reads the engine key (upstream_naming.py).'],adaptations=['JSON, Cypher, Canvas and all owned vault file contents are compared exactly. GraphML is compared after independent NetworkX parsing, including attribute types.','SVG uses native XML primitives and the same force/energy objectives; Matplotlib font metrics, tight bounding boxes and the Rust large-graph optimizer iterates may differ.','Force-directed positions are compared with NetworkX spring_layout at an absolute tolerance of 1e-8. On the Linux x86-64 qualification host one coordinate of 22 differs by 2.2e-9 from NetworkX, the same values with the 0.1.6 build against Graphify 0.9.91 and with this build against 0.9.95: floating-point rounding on that host, not a change of behaviour (a 2e-9 tolerance before).','The Rust malformed-graph status is a typed enum rather than a Python identity sentinel. Filename budgets can be supplied explicitly by callers; original monkeypatch budget fixtures use that dependency seam. In-memory neighbor order is retained explicitly across the development JSON bridge.','Python tests remain reference files. Existing native atomic-writer checks cover the shared replacement implementation.'])
finally:
    for name,value in saved.items():setattr(reference,name,value)
    native.close()
