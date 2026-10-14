#!/usr/bin/env python3
"""Unchanged analysis contracts plus independent native graph-algorithm checks."""
import os
import sys

# Upstream's CLI runs extract, update and cluster-only with PYTHONHASHSEED=0
# (#3641), which fixes the order networkx takes cycle-search pivots from sets
# of file paths; the port replays that order, so compare under the same seed.
if os.environ.get('PYTHONHASHSEED') != '0':
    os.execvpe(sys.executable, [sys.executable, *sys.argv], {**os.environ, 'PYTHONHASHSEED': '0'})
import functools
import importlib.util
import inspect
import json
import math
import random
from contextlib import ExitStack
from unittest.mock import patch
import networkx as nx
from parity import Native, SOURCE, upstream_tests
import graphify.analyze as reference

spec=importlib.util.spec_from_file_location('_graphify_analyze_oracle',SOURCE/'graphify/analyze.py')
oracle=importlib.util.module_from_spec(spec);oracle.__package__='graphify';spec.loader.exec_module(oracle)
native=Native('analyze');counts={};expected_errors=[];bounded_cases=0
def data(graph):return nx.node_link_data(graph,edges='edges')
def norm(value):
    if isinstance(value,dict) and 'new_nodes' in value:
        value={**value,'new_nodes':sorted(value['new_nodes'],key=lambda x:repr(x['id'])),'removed_nodes':sorted(value['removed_nodes'],key=lambda x:repr(x['id']))}
    # Cycles and bridge-question community lists compare exactly: networkx's
    # cycle pivots come from sets of paths (fixed under PYTHONHASHSEED=0) and
    # the questions iterate a set of community ids (ints and None); the port
    # replays both orders (python_compat::set_order).
    return value
def wrapped(name,operation):
    original=getattr(oracle,name);signature=inspect.signature(original)
    @functools.wraps(getattr(reference,name))
    def run(*args,**kwargs):
        bound=signature.bind(*args,**kwargs);bound.apply_defaults();request={}
        for key,value in bound.arguments.items():
            if key in ('G','G_old'):
                request['graph']=data(value);request['neighbor_order']=[[n,list(value.neighbors(n))] for n in value]
            elif key=='G_new':request['new_graph']=data(value)
            elif key=='node_id':request['node']=value
            elif key in ('node_community','degrees'):request[key]=None if value is None else list(value.items())
            elif key=='src_a':request['a']=value
            elif key=='src_b':request['b']=value
            else:request[key]=value
        if request.get('communities') is None and 'communities' in request:request['communities']={}
        counts[operation]=counts.get(operation,0)+1
        try:expected=original(*args,**kwargs)
        except Exception as error:
            actual=native.call(operation,**request)
            if actual.get('ok'):native.fail(operation=operation,request=request,expected_error=repr(error),actual=actual)
            expected_errors.append((operation,type(error).__name__,actual['error']));raise
        actual=native.result(operation,**request)
        if name=='_surprise_score':actual=tuple(actual)
        if name=='_node_community_map':actual=dict(actual)
        if norm(actual)!=norm(expected):native.fail(operation=operation,request=request,expected=expected,actual=actual)
        return actual
    return run
operations={
    'god_nodes':'god_nodes','surprising_connections':'surprises','_cross_file_surprises':'cross_file',
    '_cross_community_surprises':'cross_community','_surprise_score':'surprise_score','suggest_questions':'questions',
    '_cross_language':'cross_language','_file_category':'file_category','_top_level_dir':'top_level_dir',
    '_node_community_map':'node_community_map','_is_file_node':'is_file','_is_concept_node':'is_concept',
    '_is_json_key_node':'is_json_key','graph_diff':'diff','find_import_cycles':'cycles'}
functions={name:wrapped(name,'analyze.'+operation) for name,operation in operations.items()}
def centrality(graph,sample=None):
    counts['centrality']=counts.get('centrality',0)+1
    actual=native.result('analyze.centrality',graph=data(graph),sample=sample,neighbor_order=[[n,list(graph.neighbors(n))] for n in graph])
    expected=nx.betweenness_centrality(graph,k=sample,seed=42)
    for node,value in actual['nodes']:
        if not math.isclose(value,expected[node],rel_tol=2e-13,abs_tol=2e-15):native.fail(operation='node centrality',node=node,expected=expected[node],actual=value,graph=data(graph))
    if not graph.is_multigraph():
        expected=nx.edge_betweenness_centrality(graph,k=sample,seed=42)
        for a,b,value in actual['edges']:
            if not math.isclose(value,expected[a,b],rel_tol=2e-13,abs_tol=2e-15):native.fail(operation='edge centrality',edge=[a,b],expected=expected[a,b],actual=value,graph=data(graph))
try:
    with ExitStack() as stack:
        for name,fn in functions.items():stack.enter_context(patch.object(reference,name,fn))
        tests=upstream_tests('tests/test_analyze.py','tests/test_god_nodes_exclude_hubs.py','tests/test_god_nodes_cli.py',
            'tests/test_swift_builtin_noise.py::test_god_nodes_excludes_swift_builtin_labels',
            'tests/test_type_only_import_cycles.py::test_the_reporters_two_file_cycle_is_gone',
            'tests/test_type_only_import_cycles.py::test_a_real_runtime_cycle_is_still_reported',
            'tests/test_type_only_import_cycles.py::test_a_cycle_with_one_type_only_leg_is_not_a_cycle',
            'tests/test_cross_extension_reexport_self_cycle.py::test_cross_ext_reexport_no_phantom_import_cycle',
            'tests/test_js_import_resolution.py::test_ts_dynamic_import_does_not_create_phantom_cycle',
            'tests/test_semantic_similarity.py::test_semantic_edge_scores_higher_than_references',
            'tests/test_semantic_similarity.py::test_semantic_edge_reason_mentions_similarity',
            'tests/test_suggest_questions_ambiguous.py')
    rng=random.Random(2026092349)
    # Root-level files, './' prefixes, backslashes and absolute paths exercise
    # the 0.9.73 top-level-directory normalisation.
    paths=['a.py','b.pyw','src/a.tsx','lib/b.kt','a.TS','foo.c','README.md','.py','C:\\a.rs','foo/bar.PDF','中文.JPEG','foo.ext/empty','x.','a.swift',
        './c.py','././lib/d.kt','src\\e.py','.\\src\\f.rs','/abs/g.py']
    for a in paths:
        functions['_file_category'](a);functions['_top_level_dir'](a)
        for b in paths:functions['_cross_language'](a,b)
    for index in range(250):
        kind=rng.choice([nx.Graph,nx.DiGraph,nx.MultiGraph,nx.MultiDiGraph]);graph=kind();n=rng.randrange(1,40)
        ids=[f'n{i}' for i in range(n)];rng.shuffle(ids)
        for node in ids:
            source=rng.choice(paths+['',None,'concept'])
            name=rng.choice([node,node+'()',source or '', 'Path','.stub()','start','Data','name',''])
            # merge-graphs / global add tag nodes with their repo (0.9.73 cross-repo key).
            repo={'repo':rng.choice(['alpha','beta',None])} if rng.random()<0.3 else {}
            # AST/semantic tiers exercise the 0.9.81 contains-only gap rule (#4205).
            tier={'_origin':rng.choice(['ast','semantic'])} if rng.random()<0.4 else ({'source_location':'L3'} if rng.random()<0.3 else {})
            graph.add_node(node,label=name,source_file=source,file_type=rng.choice(['code','document','rationale']),**repo,**tier)
        for _ in range(rng.randrange(n*3)):
            u,v=rng.choices(ids,k=2)
            graph.add_edge(u,v,relation=rng.choice(['calls','uses','contains','imports','imports_from','re_exports','method','references','semantically_similar_to']),
                confidence=rng.choice(['EXTRACTED','INFERRED','AMBIGUOUS','other']),source_file=rng.choice(paths),
                _src=rng.choice([u,v,'ghost']),_tgt=rng.choice([u,v,'ghost']),type_only=rng.random()<0.1,deferred=rng.random()<0.1)
        communities={}
        for node in ids:
            if rng.random()<0.9:communities.setdefault(rng.randrange(6),[]).append(node)
        functions['_node_community_map'](communities)
        for node in ids[:5]:
            for method in ['_is_file_node','_is_concept_node','_is_json_key_node']:functions[method](graph,node)
        functions['god_nodes'](graph,rng.choice([-2,0,1,3,10,99]),rng.choice([None,0,50,90,100]))
        for method in ['surprising_connections','_cross_file_surprises','_cross_community_surprises']:
            try:functions[method](graph,communities,rng.choice([-2,0,1,5,999]))
            except ValueError:
                if not graph.is_multigraph():raise
        functions['suggest_questions'](graph,communities,{c:f'Group {c}' for c in communities},999)
        functions['find_import_cycles'](graph,5,10000)
        centrality(graph)
        changed=graph.copy();changed.remove_node(ids[0]);changed.add_node('fresh',label='New')
        # Reversed stored directions (#4067): removed + added only when both sides mark them.
        for _u,_v,d in changed.edges(data=True):
            if rng.random()<0.25:d['_src'],d['_tgt']=d['_tgt'],d['_src']
            elif rng.random()<0.1:d.pop('_src',None)
        functions['graph_diff'](graph,changed)
    for size in [1001,1100,2500]:
        graph=nx.disjoint_union_all([nx.path_graph(10) for _ in range(size//10)])
        graph.add_nodes_from(range(len(graph),size))
        for node in graph:graph.nodes[node].update(label=f'Symbol{node}',source_file='source.py')
        centrality(graph,100);functions['suggest_questions'](graph,{i:list(range(i,min(i+10,size))) for i in range(0,size,10)},{},30)
    for count in [2,4,7,12]:
        graph=nx.DiGraph()
        for i in range(count):graph.add_node(str(i),source_file=f'f{i}.ts')
        for i in range(count):
            for j in range(count):graph.add_edge(str(i),str(j),relation='imports_from',source_file=f'f{i}.ts')
        for bound in [0,1,2,3,5]:
            for top in [1,3,10]:
                records=native.result('analyze.cycles',graph=data(graph),max_cycle_length=bound,top_n=top);bounded_cases+=1
                assert len(records)<=top
                seen=set()
                for row in records:
                    cycle=row['cycle'];assert len(cycle)==len(set(cycle))<=bound;assert cycle[0]==min(cycle)
                    assert tuple(cycle) not in seen;seen.add(tuple(cycle))
                # A capped search keeps the cycles found first, which depends
                # on the pivots networkx takes from sets; under the CLI's
                # PYTHONHASHSEED=0 they are fixed, so the capped result
                # matches exactly.
                expected=oracle.find_import_cycles(graph,bound,top)
                if records!=expected:native.fail(operation='bounded cycles',bound=bound,top=top,expected=expected,actual=records,graph=data(graph))
    for count in [0,1,30,5001]:
        graph=nx.empty_graph(count)
        if count>1:graph.add_edge(0,1)
        functions['_cross_community_surprises'](graph,{},5)
    native.receipt('graphify/analyze.py',['src/analyze.rs','src/analyze_data.rs','src/graph_algorithms.rs','src/python_compat.rs'],
        upstream_tests=tests,operations=counts,bounded_cycle_cases=bounded_cases,expected_errors=expected_errors,
        adaptations=['Python CLI/extraction/build fixtures call native analysis; this is not yet full native CLI/build qualification.',
            'Node-difference ordering is deterministic in Rust; upstream derives it from unordered sets of node ids. Bridge-question community lists follow the CPython 3.12 order of the upstream set of community ids and compare exactly.',
            'Cycle search follows networkx 3.7 (SCC order, Gupta–Suzumura enumeration, the 10*top_n candidate cap, rotation deduplication), including the pivots and subgraph-view node orders it takes from sets of file paths under the PYTHONHASHSEED=0 upstream\'s CLI sets (#3641); this check runs under that seed and compares cycles in order, capped searches included.',
            'Parallel-edge no-community surprise requests preserve the upstream keyed-edge unpacking failure.',
            'Centrality scores compare with tight floating-point tolerances; question and surprise text compare exactly.'])
finally:native.close()
