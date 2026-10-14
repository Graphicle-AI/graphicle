#!/usr/bin/env python3
"""Native wiki: original contracts plus byte-identical complete directory output."""
import functools,inspect,io,json,os,random,re,shutil,sys,tempfile,types
from pathlib import Path
from contextlib import ExitStack,redirect_stderr
from unittest.mock import patch
import networkx as nx
from parity import Native,SOURCE,upstream_tests
from naming import to_upstream
from upstream_fixes import oracle_source
import graphify.wiki as reference
# Oracle = the pinned wiki.py. PR 3095 (index links by written slug) landed in 0.9.88.
oracle=types.ModuleType('_wiki_oracle');oracle.__package__='graphify';exec(compile(oracle_source('graphify/wiki.py'),'<pinned wiki.py>','exec'),oracle.__dict__)
native=Native('wiki');counts={};written=0;errors=0
def snapshot(path):return {p.name:p.read_bytes().decode() for p in Path(path).iterdir() if p.is_file()}
def wrapper(name,op):
    original=getattr(oracle,name);sig=inspect.signature(original)
    @functools.wraps(getattr(reference,name))
    def call(*args,**kwargs):
        global written,errors
        native.use_working_directory(os.getcwd());bound=sig.bind(*args,**kwargs);bound.apply_defaults();r=dict(bound.arguments)
        if 'G' in r:
            g=r.pop('G');r['graph']=nx.node_link_data(g,edges='links');r['neighbor_order']=[[n,list(g[n])] for n in g]
        if 'output_dir' in r:r['output_dir']=str(r['output_dir'])
        counts[op]=counts.get(op,0)+1;stderr=io.StringIO()
        # Both implementations start from the same directory state: since
        # 0.9.69 an unchanged page is not rewritten and orphans are swept by
        # name, so on a case-insensitive filesystem a run is not idempotent
        # (a page kept under another case is swept, then recreated next run).
        twin=None
        if name=='to_wiki':
            twin=Path(tempfile.mkdtemp(prefix='graphify-wiki-native-'))/'wiki'
            if Path(r['output_dir']).exists():shutil.copytree(r['output_dir'],twin)
            r['output_dir']=str(twin)
        try:
            with redirect_stderr(stderr):expected=original(*args,**kwargs)
        except ValueError as e:
            actual=to_upstream(native.call(op,**r))
            if twin:shutil.rmtree(twin.parent,ignore_errors=True)
            if actual.get('ok') or str(e)!=actual.get('error'):native.fail(operation=op,request=r,expected_error=str(e),actual=actual)
            errors+=1;raise
        before=snapshot(bound.arguments['output_dir']) if name=='to_wiki' else None
        actual=to_upstream(native.result(op,**r))
        if name=='to_wiki':
            # Written pages are compared byte for byte after the name translation.
            warnings=actual['warnings'];actual=actual['count'];after=to_upstream(snapshot(twin));written+=len(after);shutil.rmtree(twin.parent)
            if before!=after:native.fail(operation=op,request=r,expected=before,actual=after)
            if warnings!=stderr.getvalue().splitlines():native.fail(operation=op,expected_warnings=stderr.getvalue().splitlines(),actual_warnings=warnings)
            for w in warnings:print(w,file=sys.stderr)
        if name=='_cross_community_links':actual=[tuple(row) for row in actual]
        if actual!=expected:native.fail(operation=op,request=r,expected=expected,actual=actual)
        return actual
    return call
fns={name:wrapper(name,'wiki.'+op) for name,op in {'_safe_filename':'safe_name','_md_link':'link','_cross_community_links':'cross','_community_article':'community','_god_node_article':'god','_index_md':'index','to_wiki':'write'}.items()}
try:
    with ExitStack() as stack:
        for name,fn in fns.items():stack.enter_context(patch.object(reference,name,fn))
        tests=upstream_tests('tests/test_wiki.py','tests/test_wiki_link_filename_parity.py','tests/test_wiki_truncation_indicator.py')
    rng=random.Random(2026092354);labels=['A','a','__init__()','Type[T]','Ünicode Straße','文档 索引','C# & Auth (v2)','100% coverage','../','index','x\x07y','x|y','name?', 'a/b:c']
    for label in labels:
        for limit in [-5,-1,0,1,20,200]:fns['_safe_filename'](label,limit)
    with tempfile.TemporaryDirectory(prefix='graphify-wiki-') as tmp:
        out=Path(tmp)/'wiki';out.mkdir()
        for i in range(150):
            g=rng.choice([nx.Graph,nx.DiGraph,nx.MultiGraph,nx.MultiDiGraph])();communities={};community_labels={};cohesion={}
            for n in range(rng.randrange(1,35)):
                node=str(n);g.add_node(node,label=rng.choice(labels),source_file=rng.choice(['a.py','b.py',None,'','é.md']),file_type='code')
                cid=rng.randrange(5);communities.setdefault(cid,[]).append(node);community_labels[cid]=rng.choice(labels);cohesion[cid]=rng.random()
            ns=list(g)
            for _ in range(rng.randrange(70)):g.add_edge(rng.choice(ns),rng.choice(ns),relation=rng.choice(['calls','references','unknown']),confidence=rng.choice(['EXTRACTED','INFERRED','AMBIGUOUS']),weight=1.0)
            mapping={n:c for c,ns in communities.items() for n in ns};resolver={'index':'index',**{l:'Article'+str(i) for i,l in enumerate(labels)}}
            gods=[{'id':n,'label':g.nodes[n]['label'],'degree':g.degree(n)} for n in list(g)[:3]]
            for cid,nodes in communities.items():
                fns['_cross_community_links'](g,nodes,cid,community_labels,mapping)
                fns['_community_article'](g,cid,nodes,community_labels[cid],community_labels,cohesion[cid],mapping,resolver)
            fns['_god_node_article'](g,list(g)[0],community_labels,mapping,resolver)
            community_slugs={cid:'Community'+str(cid) for cid in communities if rng.random()<0.8};god_articles=[(n,'God'+str(i)) for i,n in enumerate(list(g)[:3]) if rng.random()<0.8]+([(list(g)[0],'GodAgain')] if rng.random()<0.3 else [])
            fns['_index_md'](communities,community_labels,gods,g.number_of_nodes(),g.number_of_edges(),community_slugs,god_articles)
            fns['_md_link'](rng.choice(labels),resolver)
            (out/'stale.md').write_text('stale');(out/'retain.txt').write_text('keep');fns['to_wiki'](g,communities,out,community_labels,cohesion,gods)
        for communities in [{},{0:['missing']}]:
            try:fns['to_wiki'](nx.Graph(),communities,out)
            except ValueError:pass
        # #3032/#3033 (PR 3095, upstream since 0.9.88): identically labelled communities and god nodes each keep an index link to their own file.
        g=nx.Graph();g.add_node('n1',label='GameState',file_type='code',source_file='a.py');g.add_node('n2',label='GameState',file_type='code',source_file='b.py');g.add_edge('n1','n2',relation='references',confidence='INFERRED',weight=1.0)
        gods=[{'id':'n1','label':'GameState','degree':1},{'id':'n2','label':'GameState','degree':1}]
        n=fns['to_wiki'](g,{0:['n1'],1:['n2']},out,{0:'Core',1:'Core'},{},gods)
        targets=sorted(re.findall(r'\]\(([^)]+\.md)\)',(out/'index.md').read_text()))
        assert n==4 and targets==['Core.md','Core_2.md','GameState.md','GameState_2.md'],(n,targets)
        assert all((out/t).exists() for t in targets),targets
    native.receipt('graphify/wiki.py',['src/wiki.rs'],upstream_tests=tests,operations=counts,exact_written_file_comparisons=written,expected_errors=errors,
        adaptations=['The JSON adapter returns successful-export warnings for the caller to display; original tests receive those native warnings.', 'Graph neighbor order is supplied with the node-link snapshot so stable degree ties and links preserve source order.'])
finally:native.close()
