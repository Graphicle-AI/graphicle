#!/usr/bin/env python3
"""Compare complete parked-member call linking and original merge tests."""
import random
from parity import Native, upstream_tests
from upstream_cli import in_process_graphify
import graphify.cross_repo_calls as reference
import networkx as nx
from networkx.readwrite import json_graph

native=Native("cross_repo_calls")
original=reference.link_cross_repo_member_calls
counts={"links":0,"key":0,"suffix":0,"entries":0,"member_relations":0,"member_keys":0,"language_suffixes":0}
def link(graph):
    counts["links"]+=1
    expected_graph=graph.copy();added=original(expected_graph)
    actual=native.result("cross_calls.link",graph=json_graph.node_link_data(graph,edges="links"))
    expected={"added":added,"graph":json_graph.node_link_data(expected_graph,edges="links")}
    if actual!=expected:native.fail(input=json_graph.node_link_data(graph,edges="links"),expected=expected,actual=actual)
    rebuilt=json_graph.node_link_graph(actual["graph"],edges="links")
    graph.clear();graph.graph.update(rebuilt.graph);graph.add_nodes_from(rebuilt.nodes(data=True));graph.add_edges_from(rebuilt.edges(keys=True,data=True) if graph.is_multigraph() else rebuilt.edges(data=True))
    return actual["added"]
def compare(name,operation,expected,**request):
    counts[name]+=1
    actual=native.result(operation,**request)
    if isinstance(expected,tuple):expected=list(expected)
    if actual!=expected:native.fail(operation=operation,request=request,expected=expected,actual=actual)
try:
    reference.link_cross_repo_member_calls=link
    with in_process_graphify():tests=upstream_tests("tests/test_cross_repo_member_calls.py","tests/test_cross_repo_external_call_guards.py")
    rng=random.Random(2026092312)
    for _ in range(750):
        graph=rng.choice([nx.Graph,nx.DiGraph,nx.MultiGraph,nx.MultiDiGraph])()
        for i in range(rng.randrange(1,25)):
            graph.add_node(f"n{i}",label=rng.choice(["Type","Greeter","Other",".run()",".greet()","Run()","-greet","+greet","-run"]),source_file=rng.choice(["main.java","lib.java","one.cs","a.h","x.cpp","a.swift","a.py","","Upper.CS","a.go","b.kt","c.m","d.php","e.ts","f.mjs","g.vue"]),repo=rng.choice(["a","b","c","",None]),_callable_class=rng.choice([True,False,False]),file_type="code",metadata={"namespace":"Ns","unresolved_calls":[{"callee":rng.choice(["run","greet","Run","","+greet","-greet"]),"receiver_type":rng.choice(["Type","Greeter","Other"]),"lang":rng.choice(["cpp","csharp","java","swift","rust","go","kotlin","objc","php","typescript"]),"line":"L23"}]})
        for _ in range(30):
            u,v=rng.choices(list(graph),k=2)
            graph.add_edge(u,v,relation=rng.choice(["method","defines","calls","contains"]),_cross_repo_call=rng.random()<.15)
        link(graph);link(graph)
    values=[None,0,False,[],{},"",".Run()"," .Run() ","...run()()","src/file.cs","src/file.",".bashrc","..", "...x", "A.CPP","dir.with.dot/file","folder\\file.CS","ΣΣ.CS"]
    for _ in range(500):values.append("".join(rng.choices(list("ab.CS()/\\ ")+["Σ","é","\x1c"],k=rng.randrange(40))))
    for value in values:
        compare("key","cross_calls.key",reference._key(value),value=value)
        compare("suffix","cross_calls.suffix",reference._suffix(value),value=value)
        for metadata in [value,{"unresolved_calls":value}]:compare("entries","cross_calls.entries",reference._parked_entries({"metadata":metadata}),data={"metadata":metadata})
    for lang in ["cpp","csharp","java","swift","go","kotlin","objc","php","typescript","unknown",""]:
        compare("member_relations","cross_calls.member_relations",reference._member_relations(lang),language=lang)
        compare("language_suffixes","cross_calls.language_suffixes",sorted(reference._LANG_SUFFIXES.get(lang,())),language=lang)
        for callee in ["greet","-greet","+greet","+-greet","--x","",".run()"]:compare("member_keys","cross_calls.member_keys",reference._member_keys(lang,callee),language=lang,callee=callee)
finally:native.close()
native.receipt("graphify/cross_repo_calls.py",["src/cross_repo_calls.rs"],checked=counts,upstream_tests=tests,
    comparisons="Cross-repository/language guards, C# namespace visibility, C++ declaration fallback, member precedence/ambiguity, stale-output removal, repeated runs, exact graph mutations and helper boundaries.",
    test_scope="Original merge CLI fixture runs in process so its call-link callback is native; source extraction and merge orchestration remain upstream Python. Rust linking, graph operations and C# resolution contain no Python invocation.")
