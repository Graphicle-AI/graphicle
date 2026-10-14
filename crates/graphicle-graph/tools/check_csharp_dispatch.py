#!/usr/bin/env python3
"""Compare C# interface dispatch against the pinned resolver and its tests."""
import copy
import dataclasses
import random
from parity import Native, upstream_tests
import graphify.csharp_dispatch as reference
import graphify.interface_dispatch as mechanism
import graphify.extract as extraction
import graphify.resolver_registry as registry

native=Native("csharp_dispatch")
original=reference.resolve_csharp_interface_dispatch
original_label=reference._method_label
counts={"resolve":0,"method_label":0,"is_csharp":0}
def resolve(per_file,nodes,edges):
    counts["resolve"]+=1
    expected_nodes=copy.deepcopy(nodes);expected_edges=copy.deepcopy(edges)
    original(per_file,expected_nodes,expected_edges)
    # This resolver consults only each per-file record's source_file field.
    projected=[{"source_file":r.get("source_file","")} if isinstance(r,dict) else r for r in per_file]
    actual=native.result("csharp_dispatch.resolve",per_file=projected,nodes=nodes,edges=edges)
    expected={"nodes":expected_nodes,"edges":expected_edges}
    if actual!=expected:native.fail(per_file=projected,nodes=nodes,edges=edges,expected=expected,actual=actual)
    nodes[:]=actual["nodes"];edges[:]=actual["edges"]
def label(node):
    counts["method_label"]+=1
    expected=original_label(node);actual=native.result("csharp_dispatch.method_label",node=node)
    if actual!=expected:native.fail(node=node,expected=expected,actual=actual)
    return actual
def is_csharp(node):
    counts["is_csharp"]+=1
    # 0.9.76 moved the gate into graphify.interface_dispatch.declared_in.
    expected=mechanism.declared_in(node,(".cs",));actual=native.result("csharp_dispatch.is_csharp",node=node)
    if actual!=expected:native.fail(node=node,expected=expected,actual=actual)
    return actual
try:
    reference.resolve_csharp_interface_dispatch=resolve;reference._method_label=label
    extraction.resolve_csharp_interface_dispatch=resolve
    registry._REGISTRY[:]=[dataclasses.replace(r,resolve=resolve) if r.name=="csharp_interface_dispatch" else r for r in registry._REGISTRY]
    tests=upstream_tests("tests/test_csharp_interface_dispatch.py")
    reference._method_label=original_label
    rng=random.Random(2026092309)
    for _ in range(750):
        nodes=[{"id":f"n{i}","label":rng.choice([".Run()",".Build(int, string)","build",".build()","IR",None,3,False," .Σ() "]),"source_file":rng.choice(["one.cs","two.cs","a.java",None,""]),"source_location":rng.choice(["L1","L20",None])} for i in range(rng.randrange(1,25))]
        edges=[{"source":rng.choice(nodes)["id"],"target":rng.choice(nodes)["id"],"relation":rng.choice(["method","method","implements","calls","dispatches_to"])} for _ in range(40)]
        if rng.random()<.5:edges.extend(copy.deepcopy(edges[:10]))
        resolve([None,{}, {"source_file":"a.cs"}],nodes,edges)
        resolve([],nodes,edges)
        for node in nodes:label(node);is_csharp(node)
finally:native.close()
native.receipt("graphify/csharp_dispatch.py",["src/csharp_dispatch.rs"],checked=counts,upstream_tests=tests,
    comparisons="Single implementer/member guards, duplicate method edges, exact case and signature labels, language isolation, relation-scoped deduplication, source locations and unchanged call edges.",
    test_scope="Original parser/extract.py builds fixture inputs; its registered C# dispatch callback is replaced by the native Rust implementation. A native LanguageResolver factory is available for the eventual native extractor registry.")
