#!/usr/bin/env python3
"""Compare cross-repository type links, retaining upstream merge test bodies."""
import contextlib
import io
import os
import random
import subprocess
import sys
from parity import Native, upstream_tests
import graphify.cross_repo_types as reference
import networkx as nx
from networkx.readwrite import json_graph

native=Native("cross_repo_types")
original=reference.link_shared_type_declarations
calls=0
def link(graph):
    global calls
    calls+=1
    expected_graph=graph.copy();added=original(expected_graph)
    actual=native.result("graph.link_shared_types",graph=json_graph.node_link_data(graph,edges="links"))
    expected={"added":added,"graph":json_graph.node_link_data(expected_graph,edges="links")}
    if actual!=expected:native.fail(input=json_graph.node_link_data(graph,edges="links"),expected=expected,actual=actual)
    rebuilt=json_graph.node_link_graph(actual["graph"],edges="links")
    graph.clear();graph.graph.update(rebuilt.graph);graph.add_nodes_from(rebuilt.nodes(data=True))
    graph.add_edges_from(rebuilt.edges(keys=True,data=True) if graph.is_multigraph() else rebuilt.edges(data=True))
    return actual["added"]
# The original tests spawn Python CLI processes, which cannot inherit a function
# substitution. Run that same CLI in-process under captured stdio/cwd instead.
# Its merge orchestration is the unported fixture; every type join is native.
original_run=subprocess.run
def run(args,*positional,**kwargs):
    if isinstance(args,list) and args[1:3]==["-m","graphify"]:
        import graphify.__main__ as mainmod
        saved_cwd=os.getcwd();saved_argv=sys.argv
        out=io.StringIO();err=io.StringIO();code=0
        try:
            os.chdir(kwargs.get("cwd",saved_cwd));sys.argv=["graphify",*args[3:]]
            with contextlib.redirect_stdout(out),contextlib.redirect_stderr(err):
                try:mainmod.main()
                except SystemExit as exc:code=exc.code or 0
        finally:os.chdir(saved_cwd);sys.argv=saved_argv
        return subprocess.CompletedProcess(args,code,out.getvalue(),err.getvalue())
    return original_run(args,*positional,**kwargs)
try:
    reference.link_shared_type_declarations=link
    subprocess.run=run
    tests=upstream_tests("tests/test_cross_repo_shared_types.py")
    subprocess.run=original_run
    rng=random.Random(2026092308)
    for _ in range(750):
        graph=rng.choice([nx.Graph,nx.DiGraph,nx.MultiGraph,nx.MultiDiGraph])()
        for i in range(rng.randrange(1,35)):
            graph.add_node(f"n{i}",label=rng.choice(["Event","User","Settings",None,"",False]),source_file=rng.choice(["src/a.cs","src/b.java","",None]),_callable_class=rng.choice([True,True,False,None]),metadata={"namespace":rng.choice(["Contracts","Private",None,""])},repo=rng.choice(["a","b","c","",None,1,True]))
        for _ in range(rng.randrange(8)):
            u,v=rng.choices(list(graph),k=2);graph.add_edge(u,v,relation="existing")
        link(graph);link(graph)
finally:
    subprocess.run=original_run
    native.close()
native.receipt("graphify/cross_repo_types.py",["src/cross_repo_types.rs"],graph_comparisons=calls,upstream_tests=tests,
    comparisons="Namespace-plus-label grouping, sourced declaration/repository guards, no merging, same-repository exclusion, existing edges, repeat calls, all four graph classes, exact edge attributes and order.",
    test_scope="Original merge CLI test functions execute unchanged, with subprocess dispatch replaced by the same upstream CLI in-process so the native type-link callback can be installed. The CLI/merge implementation itself is still unported.")
