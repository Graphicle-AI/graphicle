#!/usr/bin/env python3
"""Compare every symbol-resolution public helper with the pinned implementation."""
import dataclasses
import json
import random
import tempfile
from pathlib import Path
from parity import Native, upstream_tests, SOURCE
import graphify.symbol_resolution as reference

native=Native("symbol_resolution")
names=["normalise_callable_label","node_is_resolvable_symbol","build_label_index","existing_edge_pairs","iter_raw_calls","parse_python_import_aliases","build_python_symbol_index","find_unique_python_symbol","resolve_python_import_guided_calls","resolve_cross_file_raw_calls","_bash_make_id","resolve_bash_source_edges"]
original={name:getattr(reference,name) for name in names}
counts={name:0 for name in names}
def plain(value):
    if isinstance(value,Path):return str(value)
    if dataclasses.is_dataclass(value):return dataclasses.asdict(value)
    if isinstance(value,dict):return {k:plain(v) for k,v in value.items()}
    if isinstance(value,(list,tuple)):return [plain(v) for v in value]
    return value
def compare(name,operation,args,kwargs=None,request=None,convert=None):
    counts[name]+=1
    kwargs=kwargs or {}
    expected=original[name](*args,**kwargs)
    actual=native.result(operation,**plain(request or {}))
    if convert:actual=convert(actual)
    if actual!=expected:
        source=Path(request["path"]).read_text(errors="replace") if request and "path" in request and Path(request["path"]).is_file() else None
        native.fail(name=name,request=plain(request),source=source,expected=plain(expected),actual=plain(actual))
    return actual
def normalize(label):return compare("normalise_callable_label","symbols.normalize",[label],request={"label":label})
def resolvable(node):return compare("node_is_resolvable_symbol","symbols.resolvable",[node],request={"node":node})
def labels(nodes,edges=None):return compare("build_label_index","symbols.labels",[nodes,edges],request={"nodes":nodes,"edges":edges})
def pairs(edges):return compare("existing_edge_pairs","symbols.pairs",[edges],request={"edges":edges},convert=lambda items:{tuple(item) for item in items})
def raw_calls(per_file):return compare("iter_raw_calls","symbols.raw_calls",[per_file],request={"per_file":per_file})
def imports(path):return compare("parse_python_import_aliases","symbols.imports",[path],request={"path":path},convert=lambda rows:{key:reference.ImportedSymbol(**value) for key,value in rows.items()})
def python_index(nodes):return compare("build_python_symbol_index","symbols.python_index",[nodes],request={"nodes":nodes},convert=lambda rows:{tuple(key):value for key,value in rows})
def unique(symbol_index,imported):return compare("find_unique_python_symbol","symbols.unique",[symbol_index,imported],request={"index":list(symbol_index.items()),"imported":imported})
def import_calls(per_file,paths,all_nodes,all_edges):
    request={"per_file":per_file,"paths":paths,"nodes":all_nodes,"edges":all_edges}
    if reference.parse_python_import_aliases is not imports:
        request["aliases_by_file"]={str(path):reference.parse_python_import_aliases(path) for path in paths}
    return compare("resolve_python_import_guided_calls","symbols.import_calls",[per_file,paths,all_nodes,all_edges],request=request)
def cross_calls(per_file,all_nodes,all_edges):return compare("resolve_cross_file_raw_calls","symbols.cross_calls",[per_file,all_nodes,all_edges],request={"per_file":per_file,"nodes":all_nodes,"edges":all_edges})
def bash_id(*parts):return compare("_bash_make_id","symbols.bash_id",parts,request={"parts":parts})
def bash_edges(per_file,paths,root,existing_edges=None):return compare("resolve_bash_source_edges","symbols.bash_edges",[per_file,paths,root,existing_edges],request={"per_file":per_file,"paths":paths,"root":root,"edges":existing_edges or []})
try:
    for name,value in zip(names,[normalize,resolvable,labels,pairs,raw_calls,imports,python_index,unique,import_calls,cross_calls,bash_id,bash_edges]):setattr(reference,name,value)
    tests=upstream_tests("tests/test_symbol_resolution.py","tests/test_inferred_confidence_rubric.py")
    rng=random.Random(20260923)
    with tempfile.TemporaryDirectory(prefix="graphify-symbol-resolution-") as directory:
        root=Path(directory);path=root/"module.py"
        cases=["from helper import transform", "from .helper import transform as tx", "from ...pkg.helper import (\n a,\n b as c,\n)",
               "def f():\n from helper import tx\n", "if True:\n from helper import x\n", "class X:\n from helper import x\n", "from helper import *", "from . import foo", "import helper",
               "from foo import x; from bar import y as x", "from helper import ｔｘ as K", "from pkg.K import ﬀ as 𝓍", "# comment\nfrom helper import foo\n", "from foo import (", "from helper import x\nnot valid python!",
               "from helper import x\nprint(f'{1+2}')", "from helper import x\nprint(f'{ {\"a\":1}[\"a\"]}')", "from helper import x\ntype Alias[T] = list[T]", "from helper import x\nmatch foo:\n case 1: pass", "\ufefffrom helper import x", "from helper import x\x00"]
        for source in cases:
            path.write_text(source);imports(path)
        for _ in range(100):
            source=rng.choice(["# α💡\n",'"""doc\nfrom ignored import fake\n"""\n',""])+"\n".join(rng.choices(cases[:14],k=rng.randrange(1,8)))
            path.write_text(source);imports(path)
        imports(root/"missing.py");imports(root)
    for _ in range(300):
        nodes=[{"id":f"n{i}","label":rng.choice(["Thing()",".run()","HELPER","doc.md","file.py","()",".()"," Σς ","K()",None,123]),"file_type":rng.choice(["code","document",None]),"source_file":rng.choice(["src/a.py","src/b.py","tests/test_a.py","pkg/helper.py","",None])} for i in range(rng.randrange(0,25))]
        per_file=[None,{},[],{"raw_calls":"bad"},{"raw_calls":[None,3,{"caller_nid":"caller","callee":rng.choice(["Thing","run","helper","ΣΣ",None]),"is_member_call":rng.choice([None,True,False]),"source_file":"src/a.py"}]}]
        edges=[{"source":"n0","target":rng.choice(["n1","n2",None]),"relation":rng.choice(["case_of","contains"])} for _ in range(rng.randrange(0,3))]
        labels(nodes);labels(nodes,edges);python_index(nodes);cross_calls(per_file,nodes,edges)
finally:native.close()
native.receipt("graphify/symbol_resolution.py",["src/symbol_resolution.rs"],checked=counts,upstream_tests=tests,
    comparisons="Callable normalization/indexing, import-guided aliases and calls, top-level scope, Python syntax errors, malformed fragments, test/proximity disambiguation, Bash source-relative imports, exact IDs and call provenance.",
    native_parser="rustpython-parser 0.4 with native bigint support; no Python subprocess or embedded interpreter.")
