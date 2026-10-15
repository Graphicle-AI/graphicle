#!/usr/bin/env python3
"""Compare native community policies against the pinned application and kernels."""
import ast
import functools
import os
import random
import types
from contextlib import ExitStack, nullcontext
from unittest.mock import patch
import networkx as nx
from parity import Native, SOURCE, upstream_tests
from upstream_fixes import oracle_source
import graphify.cluster as reference

# Pinned cluster module with the reviewed post-pin fix (PR #2388) applied in
# memory; the pinned module object stays in place for upstream test patching.
oracle = types.ModuleType('_graphify_cluster_oracle'); oracle.__package__ = 'graphify'
exec(compile(oracle_source('graphify/cluster.py'), '<pinned cluster.py with reviewed fixes>', 'exec'), oracle.__dict__)
native = Native('cluster'); counts = {}; backend = 'auto'
def key(value):
    return tuple(key(x) for x in value) if isinstance(value,list) else value
def mapping(value):
    return {int(k):v for k,v in value.items()}
def node_mapping(value):
    return None if value is None else {key(n):c for n,c in value}
def graph_data(graph):
    return nx.node_link_data(graph, edges='edges')
def context():
    return patch.object(oracle,'_native_leiden',return_value=None) if backend=='louvain' else nullcontext()
def compare(operation,fn,decode=lambda x:x,normalize=lambda x:x,**request):
    counts[operation]=counts.get(operation,0)+1
    with context():
        try: expected=fn()
        except Exception as error:
            actual=native.call(operation,**request)
            if actual.get('ok'): native.fail(operation=operation,request=request,expected_error=repr(error),actual=actual)
            raise
    actual=decode(native.result(operation,**request))
    if normalize(actual)!=normalize(expected):native.fail(operation=operation,request=request,expected=expected,actual=actual)
    return actual
scale_env=None
def follow_scale_env():
    """GRAPHIFY_AMBIGUOUS_SCALE (0.9.81, #4199) is read per call; the native
    process copies it as GRAPHICLE_AMBIGUOUS_SCALE when it starts, so a changed
    setting restarts it."""
    global scale_env
    current=os.environ.get('GRAPHIFY_AMBIGUOUS_SCALE')
    if current!=scale_env:
        native.cwd=None;native.use_working_directory(os.getcwd());scale_env=current
@functools.wraps(reference.cluster)
def cluster(graph,resolution=1.0,exclude_hubs_percentile=None,ambiguous_scale=0.5):
    follow_scale_env()
    return compare('cluster.run',lambda:oracle.cluster(graph,resolution,exclude_hubs_percentile,ambiguous_scale),mapping,
        graph=graph_data(graph),resolution=resolution,exclude_hubs_percentile=exclude_hubs_percentile,backend=backend,
        ambiguous_scale=ambiguous_scale)
@functools.wraps(reference._partition)
def partition(graph,resolution=1.0):
    return compare('cluster.partition',lambda:oracle._partition(graph,resolution),node_mapping,
        graph=graph_data(graph),resolution=resolution,backend=backend)
@functools.wraps(reference._native_leiden)
def native_leiden(graph,resolution):
    return compare('cluster.native',lambda:oracle._native_leiden(graph,resolution),node_mapping,graph=graph_data(graph),resolution=resolution)
def canonical_groups(groups):
    return sorted([sorted(map(repr,nodes)) for nodes in groups])
@functools.wraps(reference._split_community)
def split(graph,nodes):
    return compare('cluster.split',lambda:oracle._split_community(graph,nodes),normalize=canonical_groups,
        graph=graph_data(graph),nodes=nodes,backend=backend)
@functools.wraps(reference.cohesion_score)
def cohesion(graph,nodes):
    return compare('cluster.cohesion',lambda:oracle.cohesion_score(graph,nodes),graph=graph_data(graph),nodes=nodes)
@functools.wraps(reference.label_communities_by_hub)
def labels(graph,communities):
    return compare('cluster.labels',lambda:oracle.label_communities_by_hub(graph,communities),mapping,graph=graph_data(graph),communities=communities)
@functools.wraps(reference.community_member_sigs)
def sigs(communities):
    return compare('cluster.sigs',lambda:oracle.community_member_sigs(communities),mapping,communities=communities)
@functools.wraps(reference.score_all)
def scores(graph,communities):
    return compare('cluster.scores',lambda:oracle.score_all(graph,communities),mapping,graph=graph_data(graph),communities=communities)
@functools.wraps(reference.label_still_applies)
def still(cid,current_members,previous_node_community,saved_sig,current_sig):
    # 0.9.91 (#2298): a saved label stays on an equal signature or a strict majority of both member sets.
    return compare('cluster.label_still_applies',lambda:oracle.label_still_applies(cid,current_members,previous_node_community,saved_sig,current_sig),
        cid=cid,members=list(current_members),previous={str(k):v for k,v in previous_node_community.items()},saved_sig=saved_sig,current_sig=current_sig)
def remap(communities,previous):
    return compare('cluster.remap',lambda:oracle.remap_communities_to_previous(communities,previous),mapping,communities=communities,previous=list(previous.items()))

try:
    functions={'cluster':cluster,'_partition':partition,'_native_leiden':native_leiden,'_split_community':split,
        'cohesion_score':cohesion,'label_communities_by_hub':labels,'community_member_sigs':sigs,
        'score_all':scores,'remap_communities_to_previous':remap,'label_still_applies':still}
    selected=[]
    # 0.9.91 (#2298): test_labeling's cluster-only cases reach label_still_applies,
    # community_member_sigs and the hub labels through the substitutes above (the
    # reference CLI imports them from graphify.cluster per run); its LLM labelling
    # cases drive the reference with mocked backends.
    for path in ['tests/test_cluster.py','tests/test_community_hub_labels.py','tests/test_cluster_exclude_hubs.py','tests/test_cluster_ambiguous_scale.py','tests/test_labeling.py']:
        for node in ast.parse((SOURCE/path).read_text()).body:
            if isinstance(node,ast.FunctionDef) and node.name.startswith('test_') and node.name!='test_native_leiden_returns_none_when_binding_absent':
                selected.append(path+'::'+node.name)
    with ExitStack() as stack:
        for name,fn in functions.items():stack.enter_context(patch.object(reference,name,fn))
        tests=upstream_tests(*selected)
    rng=random.Random(2026092348)
    for mode in ['auto','louvain']:
        backend=mode
        fixed=[nx.Graph(),nx.empty_graph(5),nx.path_graph(12),nx.cycle_graph(25),nx.complete_graph(30),
            nx.barbell_graph(15,4),nx.star_graph(100),nx.karate_club_graph(),
            # PR #2388: re-splits at the caller's resolution; barbell(15,4) at 0.1
            # yields 2 communities (3 with the former hard-coded 1.0 re-split).
            nx.connected_caveman_graph(6,8),nx.ring_of_cliques(8,6)]
        for graph in fixed:
            for resolution in [0.1,0.4,1.0,2.5]:
                partition(graph,resolution)
                for percentile in [None,0,50,90,100]:cluster(graph,resolution,percentile)
            split(graph,list(graph))
        for sample in range(400):
            kind=rng.choice([nx.Graph,nx.DiGraph,nx.MultiGraph,nx.MultiDiGraph]);graph=kind()
            n=rng.randrange(1,90); ids=[f'n{i:03}' for i in range(n)];rng.shuffle(ids)
            for node in ids:graph.add_node(node,label=rng.choice([node+'()',node,' ',None,False,[],['a'],{'x':2},'\x1fquoted()\x1f']))
            for _ in range(rng.randrange(n*3+1)):
                u,v=rng.choices(ids,k=2);attrs={'weight':rng.choice([0.1,0.5,1,1,2,3]),'tag':rng.choice(['é','a','z'])}
                # 0.9.81 (#4199): AMBIGUOUS edge weights are scaled before partitioning.
                confidence=rng.choice([None,'EXTRACTED','INFERRED','AMBIGUOUS','AMBIGUOUS'])
                if confidence:attrs['confidence']=confidence
                if confidence=='AMBIGUOUS' and rng.random()<0.2:attrs.pop('weight')
                graph.add_edge(u,v,**attrs)
            resolution=rng.choice([0.4,1.0,1.5,2.5])
            part=partition(graph,resolution)
            # A scale of 0.0 gives zero-weight edges, on which the shared Leiden
            # kernel panics in both implementations when a node has no other
            # edge; the upstream cases cover 0.0 on a graph where it is defined.
            groups=cluster(graph,resolution,rng.choice([None,25,50,90,100]),rng.choice([0.25,0.5,0.5,1.0,2.0]))
            labels(graph,groups);sigs(groups);scores(graph,groups)
            members=rng.sample(ids,rng.randrange(n+1));split(graph,members);cohesion(graph,members+['missing']*rng.randrange(3))
            remap(groups,{node:rng.randrange(-2,20) for node in rng.sample(ids,rng.randrange(n+1))})
            if mode=='auto' and kind==nx.Graph:native_leiden(graph,resolution)
    backend='auto'
    for graph in [nx.Graph([(1,'1')]),nx.DiGraph([('a','b')]),nx.MultiGraph([('a','b')])]:native_leiden(graph,1.0)
    for resolution in [-1,0,0.01,1,10]:
        graph=nx.Graph([('a','b'),('b','c')]);native_leiden(graph,resolution);partition(graph,resolution)
    for labels_in in [{0:[]},{0:['a','a','b']},{-4:[2,10,1]},{2:['é','中','x\x00y']}]:sigs(labels_in);remap(labels_in,{})
    for _ in range(600):
        ids=[f'n{i}' for i in range(rng.randrange(1,12))]
        previous={node:rng.randrange(0,3) for node in rng.sample(ids,rng.randrange(len(ids)+1))}
        still(rng.randrange(-1,3),rng.sample(ids,rng.randrange(len(ids)+1)),previous,rng.choice([None,'a','b']),rng.choice([None,'a','b']))
    native.receipt('graphify/cluster.py',['src/cluster.rs'],upstream_tests=tests,operations=counts,
        reference_backends={'leiden':'graspologic-native 1.2.5; byte-identical native kernel source','louvain':'NetworkX 3.7'},
        adaptations=['Statically linked Rust Leiden dependency replaces optional Python imports; absent-Python-binding loader test is excluded.',
            'Optional graspologic package wrapper test and Python >=3.13-only test retain their upstream skip conditions; native backend is independently compared here.',
            'Unordered split group enumeration is compared modulo group order, as upstream native binding returns a randomized HashMap; public cluster IDs and members compare exactly.',
            'JSON transport accepts scalar and tuple node IDs; native community IDs use signed 64-bit integers.'])
finally:native.close()
