#!/usr/bin/env python3
"""Check native shared paths, atomic output bytes and graph loading."""
import ast
import json
import os
import random
import tempfile
from pathlib import Path
from parity import Native, SOURCE, upstream_tests
from naming import to_upstream
import graphify.paths as reference
from networkx.readwrite import json_graph

native=Native("paths")
originals={name:getattr(reference,name) for name in ["_is_test_path","disambiguate_ambiguous_candidates","_path_proximity_winner","is_absolute_any_platform","stem_filename_budget","write_text_atomic","write_json_atomic","load_node_link_graph","_out_from_env"]}
counts={"classifications":0,"disambiguations":0,"budgets":0,"writes":0,"json_bytes":0,"graphs":0,"out_dirs":0}
def classify(path):
    counts["classifications"]+=1
    actual=native.result("paths.classify",path=str(path) if path is not None else None)
    expected={"test":originals["_is_test_path"](path or ""),"absolute":originals["is_absolute_any_platform"](path)}
    if actual!=expected:native.fail(path=str(path),expected=expected,actual=actual)
    return actual
def disambiguate(candidates,files,call_site):
    counts["disambiguations"]+=1
    expected=originals["disambiguate_ambiguous_candidates"](candidates,files,call_site)
    actual=native.result("paths.disambiguate",candidates=candidates,files=files,call_site=call_site)
    if actual!=expected:native.fail(candidates=candidates,files=files,call_site=call_site,expected=expected,actual=actual)
    return actual
def budget(path,reserve=0,limit=200):
    counts["budgets"]+=1
    expected=originals["stem_filename_budget"](path,reserve=reserve,limit=limit)
    actual=native.result("paths.stem_budget",path=os.path.abspath(str(path)),windows=os.name=="nt",reserve=reserve,limit=limit)
    if actual!=expected:native.fail(path=str(path),expected=expected,actual=actual)
    return actual
def text_write(path,text):
    counts["writes"]+=1
    reply=native.call("paths.write_text",path=str(path),text=text)
    if not reply["ok"]:raise OSError(reply["error"])
def json_write(path,obj,indent=None,ensure_ascii=True,compact=False):
    counts["writes"]+=1
    reply=native.call("paths.write_json",path=str(path),value=obj,indent=indent,ensure_ascii=ensure_ascii,compact=compact)
    if not reply["ok"]:raise OSError(reply["error"])
    # 0.9.86 (#3413): compact=True is separators=(",", ":").
    expected=json.dumps(obj,indent=indent,ensure_ascii=ensure_ascii,separators=(",",":") if compact else None)
    actual=Path(path).read_text()
    counts["json_bytes"]+=1
    if actual!=expected:native.fail(obj=obj,indent=indent,ensure_ascii=ensure_ascii,compact=compact,expected=expected,actual=actual)
def out_dir(value):
    """paths._out_from_env (0.9.86, #4329) for one GRAPHIFY_OUT value, both sides."""
    counts["out_dirs"]+=1
    import contextlib,io
    saved=os.environ.get("GRAPHIFY_OUT")
    if value is None:os.environ.pop("GRAPHIFY_OUT",None)
    else:os.environ["GRAPHIFY_OUT"]=value
    try:
        stderr=io.StringIO()
        with contextlib.redirect_stderr(stderr):expected=originals["_out_from_env"]()
    finally:
        if saved is None:os.environ.pop("GRAPHIFY_OUT",None)
        else:os.environ["GRAPHIFY_OUT"]=saved
    actual=native.result("paths.out_dir",value=value)
    # The default folder and the setting are named after the product.
    actual_value="graphify-out" if actual["value"]==".graphicle" else actual["value"]
    warned=stderr.getvalue().strip()
    if actual_value!=expected or bool(actual["warning"])!=bool(warned):native.fail(value=value,expected=[expected,warned],actual=actual)
    if warned and to_upstream(actual["warning"].replace("'.graphicle'","'graphify-out'"))!=warned:native.fail(value=value,expected=warned,actual=actual["warning"])

try:
    reference._is_test_path=lambda p:classify(p)["test"]
    reference.is_absolute_any_platform=lambda p:classify(p)["absolute"]
    reference.disambiguate_ambiguous_candidates=disambiguate
    reference.stem_filename_budget=budget
    reference.write_text_atomic=text_write;reference.write_json_atomic=json_write
    # Python filesystem monkeypatches cannot fault a native syscall; matching
    # fault scenarios are implemented in native Rust tests rather than skipped silently.
    atomic=["test_write_text_atomic_writes_and_leaves_no_tmp","test_write_text_atomic_preserves_existing_mode","test_write_text_atomic_new_file_respects_umask","test_write_text_atomic_writes_through_symlink","test_write_json_atomic_roundtrip","test_to_json_writes_atomically_no_tmp_leftover","test_save_manifest_writes_atomically","test_write_json_atomic_ensure_ascii_false_preserves_utf8"]
    tree=ast.parse((SOURCE/"tests/test_export_path_length.py").read_text())
    budgets=["tests/test_export_path_length.py::"+n.name for n in tree.body if isinstance(n,ast.FunctionDef) and n.name.startswith("test_budget_")]
    tests=upstream_tests("tests/test_paths.py",*["tests/test_atomic_writes.py::"+name for name in atomic],*budgets)
    rng=random.Random(20260923)
    for _ in range(2000):
        path=rng.choice(["","/","\\","\\\\","C:/","1:/","\\\\?\\UNC\\","\\\\?\\C:"])+rng.choice(["/","\\"]).join(rng.choices(["tests","spec","src","contest.py","test_x.py","ThingTests.cs","a","share","?",".","..",""],k=rng.randrange(0,7)))
        classify(path)
    paths=["src/main.py","src/test_main.py","src/lib/main.py","tests/test_main.py","a/b/run.py","a/c/call.py","","/root/x.py","C:/app/x.ts","C:\\app\\test_x.ts"]
    for _ in range(500):
        files={f"n{i}":rng.choice(paths) for i in range(rng.randrange(0,12))}
        disambiguate(list(files),files,rng.choice(paths))
    with tempfile.TemporaryDirectory(prefix="graphify-paths-parity-") as directory:
        root=Path(directory)
        objects=[{},[],{"nodes":[{"id":"α💡","metadata":{"title":"\x00\x1f\x7f日本語","a":[],"b":{}}}],"values":[1.0,1e20,1e-8,1.234,True,None]},[[],{},[1,2]],{"a\nb":"value\t\\\"/"}]
        for obj in objects:
            for indent in [None,0,1,2,4]:
                for ascii in [False,True]:
                    for compact in [False,True]:json_write(root/"output.json",obj,indent,ascii,compact)
        for value in [None,"","  ","graphs"," graphs ",".","./","..","../..","a/../..","../graphs","/abs/out",". /x"]:out_dir(value)
        for multi in [False,True]:
            for directed in [False,True]:
                parallel={"nodes":[{"id":"a"},{"id":"b"}],"edges":[{"source":"a","target":"b","key":"same","weight":1},{"source":"a","target":"b","key":"different","weight":2}],"multigraph":multi,"directed":directed}
                # Arc order disagrees with node order; the legacy link carries markers.
                arcs={"nodes":[{"id":"callee"},{"id":"caller"},{"id":"legacy_b"},{"id":"legacy_a"}],"links":[{"source":"caller","target":"callee","relation":"calls"},{"source":"legacy_b","target":"legacy_a","_src":"legacy_a","_tgt":"legacy_b","relation":"calls"}],"multigraph":multi,"directed":directed}
                for data in [parallel,arcs]:
                    path=root/"graph.json";path.write_text(json.dumps(data))
                    for value in [data,str(path)]:
                        expected=json_graph.node_link_data(originals["load_node_link_graph"](value),edges="links")
                        actual=native.result("paths.load_graph",data_or_path=value)
                        counts["graphs"]+=1
                        if actual!=expected:native.fail(data=value,expected=expected,actual=actual)
finally:native.close()
native.receipt("graphify/paths.py",["src/paths.rs","src/python_json.rs","src/security.rs"],checked=counts,upstream_tests=tests,
    comparisons="Test classification, source ambiguity/proximity, cross-platform absolute paths, Windows stem budgets, byte-exact streaming JSON, Unicode text, graph formats, symlink write-through, mode preservation and temporary cleanup.",
    native_fault_tests="Permission/WinError17 recovery; preservation after writer/commit failure; final-swap rollback; same-path no-op; symlink replacement vs write-through; bounded private temporary names.",
    test_scope="Windows budget math and injected Windows failures are checked on macOS. Native Windows filesystem execution remains unqualified. Export/save_manifest callers in original tests remain Python.")
