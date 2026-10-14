#!/usr/bin/env python3
"""Check Pascal ancestor lookup and raw-call filtering against the source."""
import copy
import dataclasses
import random
from parity import Native, upstream_tests
import graphify.pascal_resolution as reference
import graphify.extract as extraction
import graphify.resolver_registry as registry

native=Native("pascal_resolution")
original=reference.resolve_pascal_inherited_calls
original_raw=reference._pascal_raw_calls
counts={"resolutions":0,"raw_calls":0}
def raw(per_file):
    counts["raw_calls"]+=1
    expected=original_raw(per_file);actual=native.result("pascal_resolution.raw_calls",per_file=per_file)
    if actual!=expected:native.fail(per_file=per_file,expected=expected,actual=actual)
    return actual
def resolve(per_file,nodes,edges):
    counts["resolutions"]+=1
    expected_nodes=copy.deepcopy(nodes);expected_edges=copy.deepcopy(edges)
    original(per_file,expected_nodes,expected_edges)
    # Only raw_calls are consumed by this file; parser objects stay in the fixture.
    projected=[{"raw_calls":r.get("raw_calls",[])} if isinstance(r,dict) else r for r in per_file]
    actual=native.result("pascal_resolution.resolve",per_file=projected,nodes=nodes,edges=edges)
    expected={"nodes":expected_nodes,"edges":expected_edges}
    if actual!=expected:native.fail(per_file=projected,nodes=nodes,edges=edges,expected=expected,actual=actual)
    nodes[:]=actual["nodes"];edges[:]=actual["edges"]
try:
    reference.resolve_pascal_inherited_calls=resolve
    extraction.resolve_pascal_inherited_calls=resolve
    registry._REGISTRY[:]=[dataclasses.replace(r,resolve=resolve) if r.name=="pascal_inherited_calls" else r for r in registry._REGISTRY]
    tests=upstream_tests("tests/test_pascal_resolution.py")
    rng=random.Random(2026092310)
    for _ in range(800):
        nodes=[{"id":f"n{i}","label":rng.choice(["Run()","Prepare()",".Run()","Other","RUN()",None,123])} for i in range(rng.randrange(1,25))]
        edges=[{"source":rng.choice(nodes)["id"],"target":rng.choice(nodes)["id"],"relation":rng.choice(["method","inherits","calls","other"])} for _ in range(45)]
        edges.extend(copy.deepcopy(edges[:10]))
        calls=[{"caller_nid":rng.choice(nodes)["id"],"callee":rng.choice(["run","prepare",".run","other","Run",None,""]),"source_file":rng.choice(["a.pas","b.pp","c.dpr","d.dpk","e.inc","f.PAS","g.cs"]),"source_location":f"L{i}"} for i in range(20)]
        per_file=[None,{}, {"raw_calls":[None,3,*calls]}]
        raw(per_file);resolve(per_file,nodes,edges);resolve(per_file,nodes,edges)
finally:native.close()
native.receipt("graphify/pascal_resolution.py",["src/pascal_resolution.rs"],checked=counts,upstream_tests=tests,
    comparisons="Nearest ancestor breadth-first lookup, cycles, ambiguity, method-edge deduplication, case contract, source suffixes, existing edge suppression, exact new edges and repeated invocation.",
    test_scope="Original Pascal extraction supplies fixture inputs; the registered inherited-call resolver executes in Rust. The single-file parser sanity test remains an upstream fixture check, not a claim that extract_pascal is ported.")
