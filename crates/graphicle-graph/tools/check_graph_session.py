#!/usr/bin/env python3
"""Resident graphs (`graph.open` + `graph_ref`) must answer exactly like inline payloads.

The pinned Python has no graph session: every call rebuilds the graph. This
check opens graphs once in the native process and compares every traversal,
formatting and clustering answer served through `graph_ref` against the
pinned functions running on the same graph, then checks session semantics that
have no oracle (naming, eviction, copy-on-write mutations, error text).
"""
import dataclasses
import json
import random
import tempfile
import types
from pathlib import Path
import networkx as nx
from networkx.readwrite import json_graph
from parity import Native
from upstream_fixes import oracle_source
import graphify.affected as affected

# Cluster oracle with the reviewed post-pin fix (PR #2388), as check_cluster.py uses.
cluster = types.ModuleType('_graphify_cluster_oracle'); cluster.__package__ = 'graphify'
exec(compile(oracle_source('graphify/cluster.py'), '<pinned cluster.py with reviewed fixes>', 'exec'), cluster.__dict__)

native = Native('graph_session'); counts = {}
def plain(value):
    if dataclasses.is_dataclass(value): return dataclasses.asdict(value)
    if isinstance(value, (list, tuple)): return [plain(v) for v in value]
    return value
def data(graph, edges='links'): return json_graph.node_link_data(graph, edges=edges)
def compare(operation, expected, **request):
    counts[operation] = counts.get(operation, 0) + 1
    actual = native.result(operation, **request)
    if actual != expected: native.fail(operation=operation, request=request, expected=expected, actual=actual)
    return actual
def expect_error(operation, needle, **request):
    counts[operation + ':error'] = counts.get(operation + ':error', 0) + 1
    response = native.call(operation, **request)
    if response.get('ok') or needle not in response.get('error', ''):
        native.fail(operation=operation, request=request, expected_error=needle, actual=response)
def random_graph(rng, kind):
    graph = kind()
    for i in range(rng.randrange(2, 40)):
        graph.add_node(f"n{i}", label=rng.choice(["foo", "foo()", "FooBar()", "a.py", "Auditoría", "résumé", None, 7]),
                       source_file=rng.choice(["src/a.py", "src/b.py", "pkg/foo.py", "", None]),
                       source_location=rng.choice(["L1", "L9", None]))
    for _ in range(rng.randrange(0, 80)):
        source, target = rng.choices(list(graph), k=2)
        graph.add_edge(source, target, relation=rng.choice(["calls", "imports", "contains", "method", "other", None]),
                       source_file=rng.choice(["src/a.py", "other.py", None]), source_location=rng.choice(["L23", None]))
    # Round-trip through node-link JSON so the oracle graph carries exactly the
    # adjacency order a resident graph is loaded with.
    return json_graph.node_link_graph(data(graph), edges='links', directed=graph.is_directed(), multigraph=graph.is_multigraph())

rng = random.Random(20260925); sessions = 0
try:
    # Caller names and automatic references occupy the same namespace. Each
    # returned reference must keep selecting its own graph until replacement
    # or close; qualification previously covered these name families apart.
    named_graph = {'directed': True, 'multigraph': False,
                   'nodes': [{'id': 'named'}], 'links': []}
    automatic_graph = {**named_graph, 'nodes': [{'id': 'automatic'}]}
    for name in ['g1', 'g2']:
        native.result('graph.open', graph=named_graph, name=name)
    automatic = native.result('graph.open', graph=automatic_graph)['graph_ref']
    if automatic in {'g1', 'g2'}:
        native.fail(operation='graph.open', expected='automatic reference distinct from live caller names', actual=automatic)
    compare('affected.resolve', 'named', graph_ref='g1', query='named')
    compare('affected.resolve', 'automatic', graph_ref=automatic, query='automatic')
    compare('graph.close', {'closed': True}, graph_ref=automatic)
    compare('affected.resolve', 'named', graph_ref='g1', query='named')
    for name in ['g1', 'g2']:
        compare('graph.close', {'closed': True}, graph_ref=name)
    for i in range(400):
        kind = rng.choice([nx.DiGraph, nx.MultiDiGraph, nx.Graph, nx.MultiGraph])
        graph = random_graph(rng, kind)
        payload = data(graph)
        opened = native.result('graph.open', graph=payload, name=f"case-{i}")
        assert opened['graph_ref'] == f"case-{i}" and opened['nodes'] == graph.number_of_nodes() and opened['edges'] == graph.number_of_edges(), opened
        assert opened['directed'] == graph.is_directed() and opened['multigraph'] == graph.is_multigraph(), opened
        ref = {'graph_ref': f"case-{i}"}; sessions += 1
        for _ in range(3):
            seed = rng.choice(list(graph)); depth = rng.randrange(-1, 6)
            relations = tuple(rng.sample(["calls", "imports", "contains", "method", "other"], rng.randrange(1, 5)))
            query = rng.choice([seed, "foo", "Foo()", "src/a.py", "resume", "missing", ""])
            compare('affected.hits', plain(affected.affected_nodes(graph, seed, relations=relations, depth=depth)),
                    seed=seed, relations=relations, depth=depth, **ref)
            compare('affected.resolve', affected.resolve_seed(graph, query, None), query=query, root=str(Path.cwd()), **ref)
            compare('affected.format', affected.format_affected(graph, query, relations=relations, depth=depth, root=None),
                    query=query, relations=relations, depth=depth, root=str(Path.cwd()), **ref)
            compare('affected.label', affected._node_label(graph, seed), seed=seed, **ref)
        if i % 4 == 0:
            expected = cluster.cluster(graph, 1.0, None)
            counts['cluster.run'] = counts.get('cluster.run', 0) + 1
            actual = native.result('cluster.run', resolution=1.0, backend='auto', **ref)
            if {int(k): v for k, v in actual.items()} != expected:
                native.fail(operation='cluster.run', graph_ref=ref, expected=expected, actual=actual)
            nodes = list(graph)
            compare('cluster.cohesion', cluster.cohesion_score(graph, nodes), nodes=nodes, **ref)
        # Copy-on-write: a mutating request works on its own copy, the resident graph is unchanged.
        if graph.number_of_nodes() >= 2 and graph.is_directed():
            a, b = rng.sample(list(graph), 2)
            mutated = graph.copy(); mutated.add_edge(a, b, relation='calls', source_location='L99')
            before = plain(affected.affected_nodes(graph, b, depth=3))
            compare('affected.hits', plain(affected.affected_nodes(mutated, b, depth=3)), seed=b, depth=3,
                    actions=[dict(kind='edge', source=a, target=b, attributes={'relation': 'calls', 'source_location': 'L99'})], **ref)
            compare('affected.hits', before, seed=b, depth=3, **ref)
        if i % 5 == 4:
            compare('graph.close', {'closed': True}, **ref)
            compare('graph.close', {'closed': False}, **ref)
            expect_error('affected.hits', 'Unknown graph_ref', seed='n0', **ref)
    # Opening from a graph.json on disk goes through the same loader as `affected.load`.
    with tempfile.TemporaryDirectory(prefix='graphify-session-') as directory:
        path = Path(directory) / 'graph.json'
        graph = random_graph(rng, nx.MultiDiGraph)
        path.write_text(json.dumps(data(graph)))
        loaded = affected.load_graph(path)
        opened = native.result('graph.open', path=str(path), name='disk')
        assert opened['nodes'] == loaded.number_of_nodes() and opened['edges'] == loaded.number_of_edges(), opened
        for seed in list(loaded)[:10]:
            compare('affected.hits', plain(affected.affected_nodes(loaded, seed, depth=4)), seed=seed, depth=4, graph_ref='disk')
        compare('affected.format', affected.format_affected(loaded, list(loaded)[0], root=None), query=list(loaded)[0], root=str(Path.cwd()), graph_ref='disk')
        path.write_text('{')
        expect_error('graph.open', '', path=str(path))
        expect_error('graph.open', '', path=str(path.parent / 'missing.json'))
    # Session semantics without an oracle: naming, listing, eviction, argument errors.
    listed = native.result('graph.list')['graphs']
    assert any(entry['graph_ref'] == 'disk' and entry['uses'] >= 12 for entry in listed), listed
    tiny = {'directed': True, 'nodes': [{'id': 'x'}, {'id': 'y'}], 'links': [{'source': 'x', 'target': 'y', 'relation': 'calls'}]}
    replaced = native.result('graph.open', graph={**tiny, 'nodes': [{'id': 'x'}], 'links': []}, name='disk')
    assert replaced['nodes'] == 1 and replaced['uses'] == 1, replaced
    compare('affected.hits', [], seed='x', graph_ref='disk')
    anonymous = [native.result('graph.open', graph=tiny)['graph_ref'] for _ in range(12)]
    assert len(set(anonymous)) == 12 and all(ref.startswith('g') for ref in anonymous), anonymous
    remaining = {entry['graph_ref'] for entry in native.result('graph.list')['graphs']}
    assert len(remaining) <= 8 and anonymous[-1] in remaining and anonymous[0] not in remaining, remaining
    expect_error('affected.hits', 'Unknown graph_ref', seed='x', graph_ref=anonymous[0])
    compare('affected.hits', [{'node_id': 'x', 'depth': 1, 'via_relation': 'calls', 'via_file': None, 'via_location': None}], seed='y', graph_ref=anonymous[-1])
    compare('cluster.partition', [['x', 0], ['y', 0]], backend='leiden', graph_ref=anonymous[-1])
    edges_payload = {**tiny, 'edges': tiny['links']}; del edges_payload['links']
    opened = native.result('graph.open', graph=edges_payload, edges_key='edges', name='edges')
    assert opened['edges'] == 1, opened
    compare('affected.hits', [{'node_id': 'x', 'depth': 1, 'via_relation': 'calls', 'via_file': None, 'via_location': None}], seed='y', graph_ref='edges')
    expect_error('graph.open', 'edges_key', graph=tiny, edges_key='nodes')
    expect_error('graph.open', 'graph or path', graph=tiny, path='x.json')
    expect_error('graph.open', 'graph or path')
    expect_error('graph.open', 'name', graph=tiny, name=3)
    expect_error('affected.hits', 'graph or graph_ref', graph=tiny, graph_ref='edges', seed='y')
    expect_error('affected.hits', 'graph_ref', seed='y', graph_ref='')
    expect_error('graph.close', 'graph_ref')
finally: native.close()
native.receipt('graphify/affected.py', ['src/session.rs', 'src/affected.rs', 'src/graph.rs', 'src/graph_node_link.rs'],
    checked=counts, resident_graphs=sessions,
    comparisons='Every affected traversal, seed resolution, report text, node label, community detection and cohesion answer served from a resident graph equals the pinned function on the same graph; mutating requests leave the resident graph unchanged.',
    scope='Graph sessions are a native-only capability: the pinned Python rebuilds a graph per call, so session semantics (mixed automatic/caller naming, independent close, LRU eviction at 8 graphs, listing, argument errors) are checked against the documented contract rather than an oracle.',
    adaptations=['Oracle graphs are round-tripped through node-link JSON so their adjacency order matches what the resident graph was loaded with.'])
