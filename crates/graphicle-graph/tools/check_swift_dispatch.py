#!/usr/bin/env python3
"""Compare Swift protocol dispatch against the pinned resolver and its tests."""
import copy
import dataclasses
import random
from parity import Native, upstream_tests
import graphify.swift_dispatch as reference
import graphify.extract as extraction
import graphify.resolver_registry as registry

native=Native("swift_dispatch")
original=reference.resolve_swift_protocol_dispatch
counts={"resolve":0}
def resolve(per_file,nodes,edges):
    counts["resolve"]+=1
    expected_nodes=copy.deepcopy(nodes);expected_edges=copy.deepcopy(edges)
    original(per_file,expected_nodes,expected_edges)
    # Like the C# gate, this resolver consults only each per-file source_file.
    projected=[{"source_file":r.get("source_file","")} if isinstance(r,dict) else r for r in per_file]
    actual=native.result("swift_dispatch.resolve",per_file=projected,nodes=nodes,edges=edges)
    expected={"nodes":expected_nodes,"edges":expected_edges}
    if actual!=expected:native.fail(per_file=projected,nodes=nodes,edges=edges,expected=expected,actual=actual)
    nodes[:]=actual["nodes"];edges[:]=actual["edges"]
try:
    reference.resolve_swift_protocol_dispatch=resolve
    extraction.resolve_swift_protocol_dispatch=resolve
    registry._REGISTRY[:]=[dataclasses.replace(r,resolve=resolve) if r.name=="swift_protocol_dispatch" else r for r in registry._REGISTRY]
    tests=upstream_tests("tests/test_swift_protocol_dispatch.py")
    rng=random.Random(2026100519)
    files=["Store.swift","RemoteStore.swift","a.cs","Store.kt",None,"","x.SWIFT","swift"]
    labels=[".fetch()",".fetch(id:)","fetch",".Fetch()","Store",None,7,True," .Σ() ",".load()"]
    for _ in range(750):
        nodes=[{"id":f"n{i}","label":rng.choice(labels),"source_file":rng.choice(files),"source_location":rng.choice(["L1","L9",None])} for i in range(rng.randrange(1,25))]
        edges=[{"source":rng.choice(nodes)["id"],"target":rng.choice(nodes)["id"],"relation":rng.choice(["method","method","implements","inherits","calls","dispatches_to"])} for _ in range(40)]
        if rng.random()<.5:edges.extend(copy.deepcopy(edges[:10]))
        resolve([None,{},{"source_file":rng.choice(files)}],nodes,edges)
        resolve([],nodes,edges)
finally:native.close()
native.receipt("graphify/swift_dispatch.py",["src/swift_dispatch.rs"],checked=counts,upstream_tests=tests,
    comparisons="Single conformer/member guards, extension conformance, protocol-extension defaults, builtin protocol stubs, cross-language isolation, relation-scoped deduplication and unchanged call edges.",
    test_scope="Original parser/extract.py builds fixture inputs; its registered Swift dispatch callback is replaced by the native Rust implementation.")
