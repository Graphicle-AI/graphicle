#!/usr/bin/env python3
"""Differential checks for the complete Markdown cross-file resolver."""
import copy
import random
from parity import Native, upstream_tests
import graphify.markdown_resolution as reference

native=Native("markdown_resolution")
original=reference.resolve_markdown_mentions
original_match=reference._match_cited_file
counts={"resolutions":0,"file_matches":0}

def resolve(per_file,all_nodes,all_edges):
    counts["resolutions"]+=1
    expected_nodes=copy.deepcopy(all_nodes)
    expected_edges=copy.deepcopy(all_edges)
    original(per_file,expected_nodes,expected_edges)
    actual=native.result("markdown.resolve_mentions",per_file=per_file,nodes=all_nodes,edges=all_edges)
    if actual!={"nodes":expected_nodes,"edges":expected_edges}:
        native.fail(per_file=per_file,nodes=all_nodes,edges=all_edges,expected={"nodes":expected_nodes,"edges":expected_edges},actual=actual)
    all_nodes[:]=actual["nodes"];all_edges[:]=actual["edges"]

def match(cited,doc_file,source_files):
    counts["file_matches"]+=1
    expected=original_match(cited,doc_file,source_files)
    actual=native.result("markdown.match_cited_file",cited=cited,doc_file=doc_file,source_files=sorted(source_files))
    if actual!=expected: native.fail(cited=cited,doc_file=doc_file,sources=sorted(source_files),expected=expected,actual=actual)
    return actual

try:
    reference.resolve_markdown_mentions=resolve
    reference._match_cited_file=match
    tests=upstream_tests("tests/test_markdown_code_spans.py")
    rng=random.Random(20260923)
    sources={"src/a.py","src/b.rs","pkg/code.py",".github/actions/x.ts","/root/code.py","/code.py","vendor/src/a.py","//share/code.py","foo/bar.md"}
    for doc in ["README.md","docs/guide.md","/guide.md","//guide.md","/root/guide.md","a//b/c.md","docs\\guide.md"]:
        for cited in ["src/a.py","./code.py","../src/a.py","../../src/a.py","/code.py","code.py","actions/x.ts",".github/actions/x.ts","src\\b.rs","",".","..","././../src/a.py","//share/code.py"]:
            match(cited,doc,sources)
    for _ in range(500):
        nodes=[{"id":"doc","file_type":"document","label":"Guide","source_file":"docs/guide.md"}]
        edges=[]
        for i in range(rng.randrange(0,25)):
            nodes.append({"id":f"n{i}","label":rng.choice(["Thing","Other",".run()","helper()","print","code.py","Thing()"]),
                "file_type":rng.choice(["code","code","document"]),"_callable":rng.choice([True,False]),"source_file":rng.choice(sorted(sources))})
            if i:
                edges.append({"source":f"n{rng.randrange(i)}","target":f"n{i}","relation":rng.choice(["contains","method","other"])})
        if len(nodes)>2 and rng.random()<.5:
            edges.extend([{"source":"n1","target":"n0","relation":"contains"},{"source":"n0","target":"n1","relation":"contains"}])
        calls=[]
        for i in range(rng.randrange(0,25)):
            call={"caller_nid":rng.choice(["doc","missing","n0"]),"callee":rng.choice(["Thing","Other","run","helper","print"]),
                "qualifiers":rng.choice([[],["Thing"],["pkg"],["Thing","Other"]]),"language":"markdown","source_file":"docs/guide.md","source_location":"L3","context":"mention"}
            if rng.random()<.5: call["path"]=rng.choice(sorted(sources))
            calls.append(call)
        per_file=[None,{}, {"raw_calls":None},{"raw_calls":[None,{},*calls]}]
        resolve(per_file,nodes,edges)
        # Running twice must not duplicate already emitted references.
        resolve(per_file,nodes,edges)
finally: native.close()

native.receipt("graphify/markdown_resolution.py",["src/markdown_resolution.rs"],checked=counts,upstream_tests=tests,
    comparisons="Complete ordered edges; explicit relative paths, hidden directories, Windows slashes, qualified ownership, ambiguous names, builtin exclusion, containment cycles and duplicate suppression.",
    test_scope="The entire Markdown resolver is native. Original code and Markdown extraction in upstream integration fixtures still produce its inputs in Python.")
