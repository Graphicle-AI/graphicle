#!/usr/bin/env python3
"""Compare the Ruby resolver, including conservative inheritance promotion."""
import copy
import dataclasses
import random
from parity import Native, upstream_tests
import graphify.ruby_resolution as reference
import graphify.extract as extraction
import graphify.resolver_registry as registry
import graphify.build as build

native=Native("ruby_resolution")
original=reference.resolve_ruby_member_calls
counts={"resolutions":0,"key":0,"method_name":0,"method_kind":0,"unsafe":0,"file_label":0,"raw_calls":0}
def resolve(per_file,nodes,edges):
    counts["resolutions"]+=1
    expected_nodes=copy.deepcopy(nodes);expected_edges=copy.deepcopy(edges)
    original(per_file,expected_nodes,expected_edges)
    projected=[{"raw_calls":r.get("raw_calls",[])} if isinstance(r,dict) else r for r in per_file]
    actual=native.result("ruby.resolve",per_file=projected,nodes=nodes,edges=edges)
    expected={"nodes":expected_nodes,"edges":expected_edges}
    if actual!=expected:native.fail(per_file=projected,nodes=nodes,edges=edges,expected=expected,actual=actual)
    nodes[:]=actual["nodes"];edges[:]=actual["edges"]
def compare(name,operation,expected,**request):
    counts[name]+=1
    actual=native.result(operation,**request)
    if actual!=expected:native.fail(function=name,request=request,expected=expected,actual=actual)
try:
    reference.resolve_ruby_member_calls=resolve;extraction.resolve_ruby_member_calls=resolve
    registry._REGISTRY[:]=[dataclasses.replace(r,resolve=resolve) if r.name=="ruby_member_calls" else r for r in registry._REGISTRY]
    tests=upstream_tests("tests/test_ruby_resolution.py")
    rng=random.Random(2026092313)
    values=[None,False,0,[],{},"", ".run()",".save!()","...((run))()","A::Thing","Root::Café",".Σ()"," Thing\n"]
    for _ in range(500):values.append("".join(rng.choices(list("Abc:_.!()/ ")+["Σ","é","\n"],k=rng.randrange(25))))
    for value in values:
        compare("key","ruby.key",reference._key(value),value=value)
        compare("method_name","ruby.method_name",reference._method_name(value),value=value)
        node={"metadata":{"ruby_method_kind":rng.choice([None,"instance","singleton","other"]),"ruby_lookup_unsafe":value,"ruby_reopened":rng.choice([False,True])}}
        compare("method_kind","ruby.method_kind",reference._ruby_method_kind(node),node=node)
        compare("unsafe","ruby.unsafe",reference._ruby_lookup_unsafe(node),node=node)
        for sf in ["src/main.rb","src\\main.rb",None]:compare("file_label","ruby.file_label",build._is_file_node_label(value,sf),label=value,source_file=sf)
    for _ in range(400):
        nodes=[{"id":"file","label":"main.rb","source_file":"main.rb","metadata":{"ruby_resolution_schema":rng.choice([1,1,1,None]),"ruby_lookup_unsafe":rng.random()<.1,"ruby_external_method_owners":rng.choice([[],[],["Base"],["Outer::*"],["::Base"]])}}]
        classes=["Base","Child","Outer::Base","Outer::Child","Module","Other","Other\n"]
        for i in range(7):
            nodes.append({"id":f"c{i}","label":rng.choice(classes),"source_file":rng.choice(["base.rb","main.rb","code.rake","bad.py"]),"metadata":{"ruby_lookup_unsafe":rng.random()<.1,"ruby_reopened":rng.random()<.1}})
        for i in range(12):nodes.append({"id":f"m{i}","label":rng.choice([".run()",".helper()",".save!()",".call()"]),"source_file":"main.rb","metadata":{"ruby_method_kind":rng.choice(["instance","singleton",None])}})
        edges=[]
        for i in range(12):edges.append({"source":f"c{rng.randrange(7)}","target":f"m{i}","relation":"method"})
        for _ in range(8):
            a,b=rng.sample(range(7),2)
            edges.append({"source":f"c{a}","target":f"c{b}","relation":"inherits","metadata":{"ruby_superclass_ref":rng.choice([nodes[b+1]["label"],"Base","::Base",None]),"ruby_lexical_scopes":rng.choice([[],["Outer"],None])}})
        calls=[]
        for i in range(20):
            caller=f"m{rng.randrange(12)}";callee=rng.choice(["run","helper","call","new","save!"])
            call={"caller_nid":caller,"callee":callee,"source_file":"main.rb","source_location":f"L{i}","is_mixin":rng.random()<.2,"is_member_call":rng.choice([True,False]),"receiver":rng.choice(["Base","Child","Outer::Child","::Base","p",None]),"receiver_type":rng.choice(["Base","Child",None])}
            calls.append(call)
            if not call["is_member_call"]:
                edges.append({"source":caller,"target":f"m{rng.randrange(12)}","relation":"calls","context":"call","confidence":"INFERRED","confidence_score":.6,"source_file":"main.rb","source_location":f"L{i}"})
        per_file=[None,{}, {"raw_calls":[None,3,*calls]}]
        compare("raw_calls","ruby.raw_calls",reference._ruby_raw_calls(per_file),per_file=per_file)
        resolve(per_file,nodes,edges);resolve(per_file,nodes,edges)
finally:native.close()
native.receipt("graphify/ruby_resolution.py",["src/ruby_resolution.rs","src/build_labels.rs"],checked=counts,upstream_tests=tests,
    comparisons="Typed receiver and constant-path lookup, method punctuation, namespace mixins, lexical superclass checks, schema/unsafe/reopened/external-owner guards, instance/singleton distinction, cycles and exact inherited-edge confidence promotion.",
    test_scope="Upstream Ruby parsing remains fixture preparation; its registered resolver is replaced with Rust. build.py's shared file-node predicate is translated as a dependency, while build.py remains pending.")
