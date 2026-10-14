#!/usr/bin/env python3
"""Run equivalent callbacks through the original and native registry drivers."""
import logging
import random
from pathlib import Path
from parity import Native
from graphify.resolver_registry import LanguageResolver, run_language_resolvers

native=Native("resolver_registry")
rng=random.Random(20260923)
logger=logging.getLogger("graphify.resolver_registry")
logger.handlers=[logging.NullHandler()];logger.propagate=False
try:
    for iteration in range(500):
        specs=[]
        for i in range(rng.randrange(0,10)):
            specs.append({"name":f"pass{i}","suffixes":rng.sample([".rs",".py",".md",".rb",""],rng.randrange(0,5)),
                "append_nodes":[{"id":f"n{i}"}],"append_edges":[{"source":f"n{i}","target":"root"}],
                "append_per_file":[{"index":i}],"error":None if rng.random()<.7 else "failure"})
        paths=rng.sample(["a.rb","a.py","a.rs","a.md","a.PY","noext","dir/a.",".md"],rng.randrange(0,8))
        trace=[];per_file=[];nodes=[];edges=[];warnings=[]
        resolvers=[]
        for spec in specs:
            def callback(per_file,nodes,edges,spec=spec):
                trace.append(spec["name"])
                per_file.extend(spec["append_per_file"]);nodes.extend(spec["append_nodes"]);edges.extend(spec["append_edges"])
                if spec["error"]:
                    warnings.append({"name":spec["name"],"error":spec["error"],"message":f'{spec["name"]} resolution failed, skipping: {spec["error"]}'})
                    raise RuntimeError(spec["error"])
            resolvers.append(LanguageResolver(spec["name"],frozenset(spec["suffixes"]),callback))
        run_language_resolvers([Path(p) for p in paths],per_file,nodes,edges,resolvers=resolvers)
        expected={"trace":trace,"per_file":per_file,"nodes":nodes,"edges":edges,"warnings":warnings}
        actual=native.result("resolvers.check_scenario",resolvers=specs,paths=paths)
        if actual!=expected:native.fail(specs=specs,paths=paths,expected=expected,actual=actual)
finally:native.close()
native.receipt("graphify/resolver_registry.py",["src/resolver_registry.rs"],scenarios=500,
    comparisons="Real native callbacks and Python callbacks with identical inputs: order, case-sensitive suffix gating, mutations, error warnings and continuation.",
    native_unit_tests="Ordered mutations and failure isolation; actual process-global register/return/copy behavior.",
    upstream_test_scope="Original extract.py's registration of unported Swift/Python resolvers is pending and is not claimed by this driver port.")
