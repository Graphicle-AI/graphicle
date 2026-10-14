#!/usr/bin/env python3
"""Differential checks for every C# name-resolution entry point and HTML helper."""
import copy
import html
import random
from parity import Native, upstream_tests
import graphify.extractors.csharp as reference
import graphify.extract as extraction

native=Native("csharp")
original={name:getattr(reference,name) for name in ["_build_csharp_type_def_index","_strip_trailing_csharp_generic_args","_resolve_cross_file_csharp_imports","_resolve_csharp_type_references","_is_cs_file","_is_dotnet_source_file","_metadata"]}
OriginalResolver=reference.CsharpNameResolver
counts={}
def compare(name,operation,expected,request):
    counts[name]=counts.get(name,0)+1
    actual=native.result(operation,**request)
    if isinstance(expected,tuple):expected=list(expected)
    if actual!=expected:native.fail(function=name,request=request,expected=expected,actual=actual)
    return actual
def index(nodes,edges=None):
    expected=original["_build_csharp_type_def_index"](nodes,edges)
    actual=native.result("csharp.index",nodes=nodes,edges=edges)
    result={tuple(key):value for key,value in actual}
    counts["index"]=counts.get("index",0)+1
    if result!=expected:native.fail(function="index",nodes=nodes,edges=edges,expected=list(expected.items()),actual=actual)
    return result
def strip(text):return compare("strip","csharp.strip_generic",original["_strip_trailing_csharp_generic_args"](text),{"text":text})
def imports(per_file,paths,nodes,edges):return mutation("_resolve_cross_file_csharp_imports","csharp.imports",per_file,paths,nodes,edges)
def references(per_file,paths,nodes,edges):return mutation("_resolve_csharp_type_references","csharp.references",per_file,paths,nodes,edges)
def mutation(name,operation,per_file,paths,nodes,edges):
    expected_nodes=copy.deepcopy(nodes);expected_edges=copy.deepcopy(edges)
    original[name](per_file,paths,expected_nodes,expected_edges)
    actual=compare(name,operation,{"nodes":expected_nodes,"edges":expected_edges},{"nodes":nodes,"edges":edges})
    nodes[:]=actual["nodes"];edges[:]=actual["edges"]
class Resolver(OriginalResolver):
    def __init__(self,nodes,edges):
        super().__init__(nodes,edges)
        self.snapshot={"nodes":copy.deepcopy(nodes),"edges":copy.deepcopy(edges)}
    def check(self,name,operation,*args,**request):
        expected=getattr(OriginalResolver,name)(self,*args)
        actual=compare(name,operation,expected,{**self.snapshot,**request})
        return tuple(actual) if isinstance(expected,tuple) else actual
    def _scopes_for(self,node,file):return self.check("_scopes_for","csharp.scopes",node,file,source_node=node,source_file=file)
    def _resolve_alias(self,label,node,file):return self.check("_resolve_alias","csharp.resolve_alias",label,node,file,label=label,source_node=node,source_file=file)
    def resolve_type_name(self,label,node,file):return self.check("resolve_type_name","csharp.resolve_type",label,node,file,label=label,source_node=node,source_file=file)
    def resolve_label(self,label,node,file):return self.check("resolve_label","csharp.resolve_label",label,node,file,label=label,source_node=node,source_file=file)
    def resolve_qualified(self,label,qualifier,node,file):return self.check("resolve_qualified","csharp.resolve_qualified",label,qualifier,node,file,label=label,qualifier=qualifier,source_node=node,source_file=file)
try:
    reference._build_csharp_type_def_index=index;reference._strip_trailing_csharp_generic_args=strip
    reference._resolve_cross_file_csharp_imports=imports;extraction._resolve_cross_file_csharp_imports=imports
    reference._resolve_csharp_type_references=references;extraction._resolve_csharp_type_references=references
    reference.CsharpNameResolver=Resolver;extraction.CsharpNameResolver=Resolver
    tests=upstream_tests("tests/test_csharp_type_resolution.py","tests/test_csharp_member_calls.py","tests/test_csharp_object_creation.py","tests/test_cross_repo_external_call_guards.py")
    # Restore Python helper lookup to avoid counting wrapper recursion as new cases.
    for name,func in original.items():setattr(reference,name,func)
    reference.CsharpNameResolver=OriginalResolver
    rng=random.Random(2026092311)
    for entity in html.entities.html5:
        for text in ["&"+entity,"prefix &"+entity+"tail suffix"]:compare("html_entities","csharp.unescape",html.unescape(text),{"text":text})
    for n in [*range(257),0xd7ff,0xd800,0xdfff,0xe000,*range(0xfdd0,0xfdf1),0xffff,0x10000,0x10ffff,0x110000,2**64]:
        for text in [f"&#{n};",f"&#x{n:x};",f"&#X{n:X}abc"]:compare("numeric_entities","csharp.unescape",html.unescape(text),{"text":text})
    for _ in range(500):
        text="".join(rng.choices(["&amp;","&notanentity;","&fake;","&gt","\r","<>","&#xFFFF;","a","𝒂"],k=10))
        compare("html_fuzz","csharp.unescape",html.unescape(text),{"text":text});strip(text)
    for _ in range(350):
        nodes=[{"id":"file","label":"main.cs","file_type":"code","source_file":"main.cs"}]
        for i in range(15):
            nodes.append({"id":f"n{i}","label":rng.choice(["Type","Other","Ns","Thing.cs",".Call()","Inner","T","",None]),"file_type":rng.choice(["code","code","document"]),"type":rng.choice(["namespace",None,None]),"source_file":rng.choice(["a.cs","z.cs","main.cs","a.java","",None]),"source_location":rng.choice(["L1","L9",None]),"metadata":{"namespace":rng.choice(["Ns","Other","",None]),"is_nested_type":rng.choice([False,False,True]),"scope_chain":rng.choice([[],["s1"],["s1","s2"]])}})
        edges=[]
        for i in range(20):
            edges.append({"source":rng.choice(nodes)["id"],"target":rng.choice(nodes)["id"],"relation":rng.choice(["references","inherits","implements","imports","defines","method","case_of"]),"source_file":rng.choice(["main.cs","x.java",None]),"metadata":{"ref_token":rng.choice(["Type","T","Other",None]),"qualified":rng.choice([False,True]),"ref_qualifier":rng.choice(["Ns","Alias","Missing",None]),"using_kind":rng.choice(["namespace","alias",None]),"target_fqn":rng.choice(["Ns","Ns.Type","Other.Type","Ns.Type&lt;T&gt;","Type"]),"alias":rng.choice(["T","Alias","Type"]),"scope_kind":rng.choice(["file","namespace"]),"scope_id":rng.choice([None,"s1","s2"])}})
        index(nodes);index(nodes,edges)
        resolver=Resolver(nodes,edges)
        for _ in range(3):
            node=rng.choice(nodes);label=rng.choice(["Type","Other","T","Alias","Missing"])
            resolver.resolve_type_name(label,node,"main.cs");resolver.resolve_qualified(label,rng.choice(["Ns","Other","Alias",None]),node,"main.cs")
            compare("namespace","csharp.namespace",OriginalResolver._namespace(node),{"nodes":[],"source_node":node})
            compare("scope_chain","csharp.scope_chain",OriginalResolver._scope_chain(node),{"nodes":[],"source_node":node})
            compare("using_in_scope","csharp.using_in_scope",resolver._using_in_scope("namespace","s1",node),{"nodes":[],"source_node":node,"kind":"namespace","id":"s1"})
        references([],[],nodes,edges);imports([],[],nodes,edges)
    for value in [None,False,0,[],{},"x.cs","x.CS","x.razor","x.cshtml","x.cs/","C:\\a.cs"]:
        for name,operation in [("_is_cs_file","csharp.is_cs"),("_is_dotnet_source_file","csharp.is_dotnet"),("_metadata","csharp.metadata")]:compare(name,operation,original[name](value),{"value":value})
finally:native.close()
native.receipt("graphify/extractors/csharp.py",["src/extractors/csharp.rs","src/html_compat.rs","src/html_entities.json"],checked=counts,upstream_tests=tests,
    comparisons="Type definition selection, namespace/import/alias scope, qualified-name shadowing, generic HTML escaping, ambiguous bindings, stub allocation/collisions/reuse, repointed stub pruning and original node/edge preservation.",
    test_scope="This upstream file contains C# resolution only. The config-driven C# parser remains in extract.py/engine.py and is not counted as converted here. Original extractor fixtures call the native resolution functions.")
