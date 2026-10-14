#!/usr/bin/env python3
"""Original global-store contracts and separate-store differential persistence checks."""
import ast
import contextlib
import datetime
import io
import json
import os
import random
import sys
import tempfile
from pathlib import Path
from unittest.mock import patch
from parity import Native, SOURCE, upstream_tests
import graphify.build as build
import graphify.global_graph as reference
import graphify.security as security
import networkx as nx
from networkx.readwrite import json_graph

native=Native("global_graph")
NOW="2026-09-23T06:30:15.123456+00:00"
class Clock:
    @staticmethod
    def now(tz=None):return datetime.datetime.fromisoformat(NOW)
reference.datetime=Clock
counts={"prefix":0,"prune":0,"tags":0,"store_actions":0,"upstream_calls":0,"graph_mutations":0}
def data(graph):return json_graph.node_link_data(graph,edges="links")
def compare(kind,expected,actual,**inputs):
    counts[kind]+=1
    expected=json.loads(json.dumps(expected))
    if actual!=expected:native.fail(kind=kind,inputs=inputs,expected=expected,actual=actual)
def result(operation,**kwargs):
    response=native.call(operation,**kwargs)
    if not response["ok"]:
        error=response["error"]
        if error.startswith("KeyError:"):raise KeyError(error)
        if error.startswith("FileNotFoundError:"):raise FileNotFoundError(error)
        raise ValueError(error)
    return response["result"]
def rebuild(graph,updated):
    loaded=json_graph.node_link_graph(updated,edges="links")
    graph.clear();graph.graph.update(loaded.graph);graph.add_nodes_from(loaded.nodes(data=True))
    graph.add_edges_from(loaded.edges(keys=True,data=True) if graph.is_multigraph() else loaded.edges(data=True))
def prefix(graph,tag,community_offset=0):
    value=result("global.prefix",graph=data(graph),tag=tag,offset=community_offset)
    counts["upstream_calls"]+=1
    return json_graph.node_link_graph(value,edges="links")
def prune(graph,tag):
    value=result("global.prune",graph=data(graph),tag=tag)
    rebuild(graph,value["graph"]);counts["upstream_calls"]+=1
    return value["removed"]
def tags(paths):
    counts["upstream_calls"]+=1
    return result("global.tags",paths=[str(p) for p in paths])
def call_store(operation,**kwargs):
    value=result(operation,directory=str(reference._GLOBAL_DIR),graph_path=str(reference._GLOBAL_GRAPH),manifest_path=str(reference._GLOBAL_MANIFEST),now=NOW,limit=security._max_graph_file_bytes(),**kwargs)
    for warning in value["warnings"]:print(warning,file=sys.stderr)
    counts["upstream_calls"]+=1
    return value["value"]
def add(path,tag):return call_store("global.add",path=str(Path(path).absolute()),tag=tag)
def remove(tag):return call_store("global.remove",tag=tag)
def listed():return call_store("global.list")
def load_manifest():return call_store("global.load_manifest")
def save_manifest(value):return call_store("global.save_manifest",value=value)
def load_graph():return json_graph.node_link_graph(call_store("global.load_graph"),edges="links")
def save_graph(graph):return call_store("global.save_graph",graph=data(graph))
def global_path():return Path(call_store("global.path"))

try:
    rng=random.Random(2026092314)
    # Graph relabeling and pruning preserve identity and ordered edge/key semantics.
    for i in range(500):
        graph=rng.choice([nx.Graph,nx.DiGraph,nx.MultiGraph,nx.MultiDiGraph])()
        for name in ["a","b","x::a","external",0,2,False,("pair",3)]:
            graph.add_node(name,label=str(name),external=rng.random()<.2,community=rng.choice([0,4,-3,True,"4",None]),source_file=rng.choice(["a.py",""]),repo=rng.choice(["a","b",None]))
        for j in range(30):
            u,v=rng.choices(list(graph),k=2)
            attrs={"relation":"calls","step":j}
            if graph.is_multigraph():graph.add_edge(u,v,key=rng.choice([0,1,"key",2.5]),**attrs)
            else:graph.add_edge(u,v,**attrs)
        graph.graph["hyperedges"]=[{"id":"h","nodes":["a",0,"external",["malformed"]]},None,{"id":7,"nodes":"x"}]
        tag=rng.choice(["x","a","repo::nested",""]);offset=rng.choice([0,5,-3])
        expected=data(build.prefix_graph_for_global(graph,tag,offset))
        actual=result("global.prefix",graph=data(graph),tag=tag,offset=offset)
        compare("prefix",expected,actual,input=data(graph),tag=tag,offset=offset)
        before=data(graph);expected_graph=graph.copy();removed=build.prune_repo_from_graph(expected_graph,tag)
        actual=result("global.prune",graph=before,tag=tag)
        compare("prune",{"removed":removed,"graph":data(expected_graph)},actual,input=before,tag=tag)
    for i in range(400):
        paths=rng.choices(["graph.json","/graph.json","src/graphify-out/graph.json","a/src/graphify-out/graph.json","b/src/graphify-out/graph.json","repo/graph.json","../graph.json","//srv/repo/graphify-out/graph.json"],k=rng.randrange(9))
        compare("tags",build.distinct_repo_tags([Path(p) for p in paths]),result("global.tags",paths=paths),paths=paths)
    for i in range(250):
        graph=rng.choice([nx.Graph,nx.DiGraph,nx.MultiGraph,nx.MultiDiGraph])()
        initial=data(graph);actions=[]
        for j in range(50):
            ids=["a","b","c",0,3]
            if rng.random()<.18:
                removed=rng.choices(ids,k=2);graph.remove_nodes_from(removed);actions.append(dict(kind="remove_nodes",ids=removed))
            elif rng.random()<.15:
                mapping={name:rng.choice(ids) for name in rng.sample(list(graph),min(2,len(graph)))}
                graph=nx.relabel_nodes(graph,mapping,copy=True);actions.append(dict(kind="relabel",mapping=list(mapping.items())))
            else:
                u,v=rng.choices(ids,k=2);attrs={"relation":"calls","step":j};key=rng.choice([0,1,"s",2.5,False])
                if graph.is_multigraph():graph.add_edge(u,v,key=key,**attrs)
                else:graph.add_edge(u,v,**attrs)
                actions.append(dict(kind="edge",source=u,target=v,key=key,attributes=attrs))
        actual=result("graph.check_scenario",data=initial,actions=actions)
        compare("graph_mutations",{"graph":data(graph),"errors":[],"edge_count":graph.number_of_edges()},actual,initial=initial,actions=actions)
    with tempfile.TemporaryDirectory(prefix="graphify-global-parity-") as tmp:
        root=Path(tmp);py_store=root/"python";rs_store=root/"rust"
        def store_paths(directory):return {"_GLOBAL_DIR":directory,"_GLOBAL_GRAPH":directory/"global-graph.json","_GLOBAL_MANIFEST":directory/"global-manifest.json"}
        with patch.multiple(reference,**store_paths(py_store)):
            for step in range(120):
                tag=rng.choice(["a","b","c",""])
                if step%7==0 and tag in reference.global_list():
                    expected=reference.global_remove(tag)
                    actual=result("global.remove",directory=str(rs_store),tag=tag,now=NOW)
                else:
                    source=root/f"{tag or 'empty'}.json"
                    if step%4==0 or not source.exists():
                        graph=rng.choice([nx.Graph,nx.DiGraph,nx.MultiGraph,nx.MultiDiGraph])()
                        for n in range(5):graph.add_node(f"n{n}",label=f"N{n}",source_file="src/x.py" if n<3 else "",external=n==4,revision=step)
                        for _ in range(8):u,v=rng.choices(list(graph),k=2);graph.add_edge(u,v,relation=rng.choice(["calls","imports"]),step=step)
                        payload=data(graph)
                        if step%3==0:payload["edges"]=payload.pop("links")
                        source.write_text(json.dumps(payload),encoding="utf-8")
                    expected=reference.global_add(source,tag)
                    actual=result("global.add",directory=str(rs_store),path=str(source),tag=tag,now=NOW)
                compare("store_actions",expected,actual["value"],step=step,tag=tag)
                compare("store_actions",reference._load_manifest(),result("global.load_manifest",directory=str(rs_store),now=NOW)["value"])
                compare("store_actions",data(reference._load_global_graph()),result("global.load_graph",directory=str(rs_store))["value"])
                for name in ["global-manifest.json","global-graph.json"]:
                    expected_text=(py_store/name).read_text();actual_text=(rs_store/name).read_text()
                    if expected_text!=actual_text:native.fail(kind="persisted_bytes",name=name,step=step,expected=expected_text,actual=actual_text)
        # Broken manifests must be retained, while valid wrong-shaped JSON stays visible to callers.
        corrupt=rs_store/"global-manifest.json";corrupt.write_text("{not json")
        value=result("global.load_manifest",directory=str(rs_store),now=NOW)
        assert value["value"]=={"version":1,"repos":{}} and "starting fresh" in value["warnings"][0]
        backups=list(rs_store.glob("global-manifest.json.corrupt.*"));assert len(backups)==1 and backups[0].read_text()=="{not json" and not corrupt.exists()
        for content in ["[]","null","7"]:
            corrupt.write_text(content);value=result("global.load_manifest",directory=str(rs_store),now=NOW)
            assert value["value"]==json.loads(content) and not value["warnings"]
        source=root/"large.json";source.write_text(json.dumps({"nodes":[],"links":[]}))
        bad=native.call("global.add",directory=str(root/"limited"),path=str(source),tag="large",limit=8)
        assert not bad["ok"] and "exceeds" in bad["error"]
        assert not native.call("global.add",directory=str(root/"missing-store"),path=str(root/"absent"),tag="x")["ok"]
        caller=nx.Graph();caller.add_node("run",label=".run()",source_file="App.java",metadata={"unresolved_calls":[{"callee":"greet","receiver_type":"Greeter","lang":"java","line":"L10"}]})
        library=nx.Graph();library.add_node("greeter",label="Greeter",source_file="Greeter.java",_callable_class=True);library.add_node("greet",label=".greet()",source_file="Greeter.java");library.add_edge("greeter","greet",relation="method")
        for tag,graph in [("app",caller),("lib",library)]:
            source=root/f"{tag}.json";source.write_text(json.dumps(data(graph)))
            with patch.multiple(reference,**store_paths(root/"py-linked")):expected=reference.global_add(source,tag)
            actual=result("global.add",directory=str(root/"rs-linked"),path=str(source),tag=tag,now=NOW)
            compare("store_actions",expected,actual["value"],tag=tag)
        assert actual["value"]["cross_repo_calls"]==1
        with patch.multiple(reference,**store_paths(root/"py-linked")):expected=data(reference._load_global_graph())
        compare("store_actions",expected,result("global.load_graph",directory=str(root/"rs-linked"))["value"])
    # Unchanged upstream tests use the Rust APIs in place of their Python implementations.
    substitutions={"global_add":add,"global_remove":remove,"global_list":listed,"_load_manifest":load_manifest,"_save_manifest":save_manifest,"_load_global_graph":load_graph,"_save_global_graph":save_graph,"global_path":global_path}
    selected=[f"tests/test_global_graph.py::{node.name}" for node in ast.parse((SOURCE/"tests/test_global_graph.py").read_text()).body if isinstance(node,ast.FunctionDef) and node.name.startswith("test_") and not node.name.startswith("test_dedup")]
    with patch.multiple(reference,**substitutions),patch.multiple(build,prefix_graph_for_global=prefix,prune_repo_from_graph=prune,distinct_repo_tags=tags):
        # 0.9.86 (#4066): global add keeps each link's stored direction and
        # rewires the markers of a deduplicated external node.
        direction=[f"tests/test_export_direction.py::{name}" for name in ("test_global_add_persists_stored_direction","test_global_add_dedup_remaps_direction_markers")]
        # 0.9.86 (#3413): global-graph.json is written as one compact line.
        tests=upstream_tests(*selected,"tests/test_global_add_tag_inference.py",*direction,"tests/test_compact_graph_json.py::test_global_graph_output_is_one_line")
finally:native.close()
native.receipt("graphify/global_graph.py",["src/global_graph.rs","src/build_global.rs","src/graph.rs"],checked=counts,upstream_tests=tests,
    comparisons="Exact ordered graph content and persisted JSON bytes across separate reference/native stores, unchanged-hash skips, updates/removals, external deduplication, ID/key collisions, corruption backups, graph byte limits, negative/missing inputs and native mutations.",
    dependencies="Only the prefix/prune/tag helpers from build.py are implemented here; build.py remains pending. All native store behavior uses the Rust graph, paths, security and cross-repo linking modules.",
    adaptations=["The library returns warnings for its CLI caller to render; the development harness renders them for unchanged upstream tests.","Native JSON/filesystem error descriptions differ while preserving failure behavior.","UTC clock and store paths are injectable for isolated tests. Production defaults are the engine's ~/.graphicle-graph directory (the reference uses ~/.graphify) and current UTC time.","JSON numbers bound integer node/community/key values; unbounded Python integers and opaque Python objects are not native API inputs."])
