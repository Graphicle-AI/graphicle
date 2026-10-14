#!/usr/bin/env python3
"""Port checks for seed resolution, reverse traversal, formatting and graph loading."""
import dataclasses
import os
import random
import tempfile
from pathlib import Path
from parity import Native, upstream_tests
import networkx as nx
from networkx.readwrite import json_graph
import graphify.affected as reference

native = Native("affected")
names = ["_normalize_label", "_bare_name", "_as_repo_relative", "_prefer_file_node", "resolve_seed", "affected_nodes", "_node_label", "_format_location", "format_affected", "load_graph"]
original = {name:getattr(reference,name) for name in names}
counts = {name:0 for name in names}
def data(graph): return json_graph.node_link_data(graph,edges="links")
def plain(value):
    if dataclasses.is_dataclass(value):return dataclasses.asdict(value)
    if isinstance(value,list):return [plain(v) for v in value]
    if isinstance(value,tuple):return [plain(v) for v in value]
    return value
def compare(name,op,args,kwargs=None,request=None):
    counts[name]+=1
    if args and isinstance(args[0],nx.Graph) and args[0].is_directed():
        request["predecessor_order"] = [[node,list(args[0].predecessors(node))] for node in args[0]]
    expected = original[name](*args,**(kwargs or {}))
    actual = native.result(op,**(request or {}))
    if actual!=plain(expected):native.fail(function=name,request=request,expected=plain(expected),actual=actual)
    return actual
def normalize(label):return compare(names[0],"affected.normalize",[label],request={"label":label})
def bare(label):return compare(names[1],"affected.bare",[label],request={"label":label})
def relative(query,root=None):return compare(names[2],"affected.relative",[query,root],request={"query":query,"root":str(root) if root is not None else os.getcwd()})
def prefer(graph,ids,query):return compare(names[3],"affected.prefer",[graph,ids,query],request={"graph":data(graph),"ids":ids,"query":query})
def resolve(graph,query,root=None):return compare(names[4],"affected.resolve",[graph,query,root],request={"graph":data(graph),"query":query,"root":str(root) if root is not None else os.getcwd()})
def hits(graph,seed,*,relations=reference.DEFAULT_AFFECTED_RELATIONS,depth=2):
    relations=tuple(relations)
    result=compare(names[5],"affected.hits",[graph,seed],dict(relations=relations,depth=depth),request={"graph":data(graph),"seed":seed,"relations":relations,"depth":depth})
    return [reference.AffectedHit(**row) for row in result]
def label(graph,node_id):return compare(names[6],"affected.label",[graph,node_id],request={"graph":data(graph),"seed":node_id})
def location(value):return compare(names[7],"affected.location",[value],request={"data":value})
def format_affected(graph,query,*,relations=reference.DEFAULT_AFFECTED_RELATIONS,depth=2,root=None):
    relations=tuple(relations)
    return compare(names[8],"affected.format",[graph,query],dict(relations=relations,depth=depth,root=root),request={"graph":data(graph),"query":query,"relations":relations,"depth":depth,"root":str(root) if root is not None else os.getcwd()})
def load(path):
    counts[names[9]]+=1
    expected=data(original[names[9]](path))
    actual=native.result("affected.load",path=str(path))
    if actual!=expected:native.fail(function="load",path=str(path),expected=expected,actual=actual)
    return json_graph.node_link_graph(actual,edges="links")
try:
    for name,func in zip(names,[normalize,bare,relative,prefer,resolve,hits,label,location,format_affected,load]):setattr(reference,name,func)
    tests=upstream_tests("tests/test_affected_cli.py","tests/test_affected_member_seed.py","tests/test_js_dynamic_import_affected.py",
        "tests/test_affected_defines.py","tests/test_affected_overrides.py")
    # Do not recursively invoke wrappers while comparing generated cases.
    for name in names:setattr(reference,name,original[name])
    rng=random.Random(20260923)
    alphabet=list("a.A_()/\\")+["é","e\u0301","ß","Σ","ς","K","İ","𝒇"]
    for _ in range(1000):
        value="".join(rng.choices(alphabet,k=rng.randrange(30)))
        normalize(value);bare(value)
    for query in [".","", "src/x.ts", "./src/x.ts", "../src/x.ts", "/repo/src/x.ts", "/repo/../x.ts", "//repo/src/x.ts", "///repo/src/x.ts", "/repo-other/x.ts", "foo\\bar", "func()"]:
        for root in [None,Path("/repo"),Path("/"),Path("."),Path("//repo")]:relative(query,root)
    for _ in range(500):
        graph=rng.choice([nx.Graph,nx.DiGraph,nx.MultiGraph,nx.MultiDiGraph])()
        for i in range(rng.randrange(1,25)):
            graph.add_node(f"n{i}",label=rng.choice(["foo","foo()","FooBar()","a.py","Auditoría","résumé","resume",None,123]),source_file=rng.choice(["src/a.py","src/b.py","pkg/foo.py","",None]),source_location=rng.choice(["L1","L9",None]))
        for _ in range(40):
            source,target=rng.choices(list(graph),k=2)
            graph.add_edge(source,target,relation=rng.choice(["calls","imports","contains","method","other","dynamic_import",None,"defines","inherits","implements","extends"]),source_file=rng.choice(["src/a.py","other.py","",None]),source_location=rng.choice(["L23","L81",None]))
        # A disk snapshot has the edge order encoded in node-link JSON.
        graph=json_graph.node_link_graph(data(graph),edges="links")
        seed=rng.choice(list(graph));depth=rng.randrange(-1,6)
        query=rng.choice([seed,"foo","Foo()","src/a.py","./src/a.py","pkg/foo.py/","resume","missing",""])
        hits(graph,seed,depth=depth);resolve(graph,query);format_affected(graph,query,depth=depth)
    mutations=0
    for _ in range(300):
        graph=nx.MultiDiGraph();graph.add_nodes_from(["a","b","c","d","e"])
        initial=data(graph);actions=[]
        for i in range(40):
            source,target=rng.choices(list(graph),k=2);key=rng.randrange(4)
            if rng.random()<.8:
                attrs={"relation":rng.choice(["calls","contains","method","imports","inherits","implements"]),"source_location":f"L{i}"}
                graph.add_edge(source,target,key=key,**attrs);actions.append(dict(kind="edge",source=source,target=target,key=key,attributes=attrs))
            else:
                try:graph.remove_edge(source,target,key)
                except nx.NetworkXError:pass
                actions.append(dict(kind="remove",source=source,target=target,key=key))
        seed=rng.choice(list(graph))
        expected=plain(original["affected_nodes"](graph,seed,depth=4))
        actual=native.result("affected.hits",graph=initial,actions=actions,seed=seed,depth=4)
        if actual!=expected:native.fail(function="mutation_traversal",initial=initial,actions=actions,seed=seed,expected=expected,actual=actual)
        mutations+=1
    with tempfile.TemporaryDirectory(prefix="graphify-affected-") as directory:
        path=Path(directory)/"graph.json"
        for text in ["{", "[]", "null"]:
            path.write_text(text)
            assert not native.call("affected.load",path=str(path))["ok"]
        assert not native.call("affected.load",path=str(path.parent/"missing"))["ok"]
finally:native.close()
native.receipt("graphify/affected.py",["src/affected.rs","src/graph.rs"],checked=counts,upstream_tests=tests,graph_mutation_traversals=mutations,
    comparisons="Exact seed selection, NFC/casefold, repo-relative paths, ambiguous names, member seeding, BFS order, cycles, parallel-edge relation/location consistency, forced-directed loading and exact report text.",
    scope="Original CLI dispatch/extraction fixtures remain Python; their affected functions are replaced by native calls. The development JSON bridge includes predecessor order for in-memory snapshots because standard node-link JSON serializes only successor order. Generated mutation tests construct both native/reference graphs directly and compare exact ordered hits without a snapshot override.",
    adaptations=["Native loader keeps the original failure category but uses native filesystem/JSON parser descriptions.","Path comparisons in this qualification use the POSIX host; Windows filesystem execution remains unqualified."])
