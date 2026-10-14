#!/usr/bin/env python3
"""Compare the shared interface-dispatch mechanism against the pinned module."""
import copy
import random
from parity import Native, upstream_tests
import graphify.interface_dispatch as reference
import graphify.csharp_dispatch as csharp
import graphify.swift_dispatch as swift

native=Native("interface_dispatch")
original=reference.resolve_interface_dispatch
original_label=reference.method_label
original_declared=reference.declared_in
counts={"resolve":0,"method_label":0,"declared_in":0}
def resolve(per_file,nodes,edges,*,suffixes):
    counts["resolve"]+=1
    expected_nodes=copy.deepcopy(nodes);expected_edges=copy.deepcopy(edges)
    original(per_file,expected_nodes,expected_edges,suffixes=suffixes)
    projected=[{"source_file":r.get("source_file","")} if isinstance(r,dict) else r for r in per_file]
    actual=native.result("interface_dispatch.resolve",per_file=projected,nodes=nodes,edges=edges,suffixes=list(suffixes))
    expected={"nodes":expected_nodes,"edges":expected_edges}
    if actual!=expected:native.fail(per_file=projected,nodes=nodes,edges=edges,suffixes=suffixes,expected=expected,actual=actual)
    nodes[:]=actual["nodes"];edges[:]=actual["edges"]
def label(node):
    counts["method_label"]+=1
    expected=original_label(node);actual=native.result("interface_dispatch.method_label",node=node)
    if actual!=expected:native.fail(node=node,expected=expected,actual=actual)
    return actual
def declared(node,suffixes):
    counts["declared_in"]+=1
    expected=original_declared(node,suffixes);actual=native.result("interface_dispatch.declared_in",node=node,suffixes=list(suffixes))
    if actual!=expected:native.fail(node=node,suffixes=suffixes,expected=expected,actual=actual)
    return actual
try:
    # The C# and Swift gates bind the mechanism by name at import time.
    reference.resolve_interface_dispatch=resolve;csharp.resolve_interface_dispatch=resolve;swift.resolve_interface_dispatch=resolve
    csharp._method_label=label
    tests=upstream_tests("tests/test_csharp_interface_dispatch.py","tests/test_swift_protocol_dispatch.py")
    csharp._method_label=original_label
    rng=random.Random(2026100523)
    files=["one.cs","Store.swift","a.java","b.kt",None,"","x.CS","cs",3]
    labels=[".Run()",".Build(int, string)","build",".build()","IR",None,3,False," .Σ() ","\x1c.fetch()\x1f",".fetch(id:)",""]
    gates=[(".cs",),(".swift",),(".cs",".swift"),(".kt",".java"),(),("",)]
    for _ in range(750):
        suffixes=rng.choice(gates)
        nodes=[{"id":f"n{i}","label":rng.choice(labels),"source_file":rng.choice(files),"source_location":rng.choice(["L1","L20",None])} for i in range(rng.randrange(1,25))]
        for node in nodes:
            if rng.random()<.1:del node["source_file"]
            if rng.random()<.1:del node["label"]
        edges=[{"source":rng.choice(nodes)["id"],"target":rng.choice(nodes)["id"],"relation":rng.choice(["method","method","implements","calls","dispatches_to"])} for _ in range(40)]
        if rng.random()<.5:edges.extend(copy.deepcopy(edges[:10]))
        resolve([None,{},{"source_file":rng.choice(files)}],nodes,edges,suffixes=suffixes)
        resolve([],nodes,edges,suffixes=suffixes)
        for node in nodes:label(node);declared(node,suffixes)
        declared(None,suffixes);declared({},suffixes)
finally:native.close()
native.receipt("graphify/interface_dispatch.py",["src/interface_dispatch.rs"],checked=counts,upstream_tests=tests,
    comparisons="Language gates (including empty and catch-all suffix sets), single implementer/member guards, method-label cuts, sourceless and cross-language ends, relation-scoped deduplication and unchanged call edges.",
    test_scope="The C# and Swift wrappers call the native Rust mechanism while the original parser/extract.py builds fixture inputs for both upstream dispatch test files.")
