#!/usr/bin/env python3
"""Call-graph accuracy on the PyCG micro-benchmark (Salis et al., ICSE 2021).

Each benchmark snippet ships a hand-written `callgraph.json` (caller -> callees,
qualified like `main.MyClass.func`). This harness extracts every snippet with
each implementation, maps the graph's `calls` edges to the same qualified names,
and reports precision/recall.

Scopes:
  function project edges whose caller is a function or method. Graphify has no
           caller for module-level statements, so this isolates how well calls
           are resolved from the edges it does try to model.
  project  edges whose caller and callee are defined in the snippet (no
           `<builtin>`, external modules or lambdas, which Graphify has no nodes
           for), including module-level callers.
  all      every expected edge (builtins, externals and lambdas count as misses).

Mapping rules (documented, applied identically to every implementation):
  * a file node is its module (`pkg/mod.py` -> `pkg.mod`, `pkg/__init__.py` -> `pkg`);
  * a definition is its module plus its `contains`/`method` parent chain;
  * a call to a class node is a call to `Class.__init__` when the graph has that
    method, and is dropped otherwise (PyCG lists no callee for a class without
    `__init__`).

Usage:
  python3 bench/accuracy/pycg_eval.py --snippets DIR --out DIR \
      [--impl native=crates/graphicle-graph/target/release/graphicle-graph]
      [--impl python=crates/graphicle-graph/.reference-venv/bin/python]
      [--impl native-precise=crates/graphicle-graph/target/release/graphicle-graph:--precise]
"""
import argparse, json, os, shutil, subprocess, sys, tempfile
from collections import defaultdict
from pathlib import Path


def module_name(rel: str) -> str:
    stem = rel[:-3] if rel.endswith(".py") else rel
    parts = [p for p in stem.split("/") if p]
    if parts and parts[-1] == "__init__":
        parts = parts[:-1]
    return ".".join(parts)


def qualified_edges(graph: dict, root: Path) -> set:
    nodes = {n["id"]: n for n in graph["nodes"]}
    parent = {}
    methods = defaultdict(set)
    for e in graph.get("links", graph.get("edges", [])):
        rel = e.get("relation")
        if rel in ("contains", "method") and e["target"] in nodes and e["source"] in nodes:
            parent.setdefault(e["target"], e["source"])
            if rel == "method":
                methods[e["source"]].add(e["target"])

    def rel_source(n):
        sf = n.get("source_file") or ""
        p = Path(sf)
        if p.is_absolute():
            try:
                p = p.relative_to(root)
            except ValueError:
                return sf
        return p.as_posix()

    def is_file_node(n):
        sf = n.get("source_file") or ""
        return bool(sf) and n.get("label") == Path(sf).name

    def bare(label: str) -> str:
        label = label or ""
        if label.startswith("."):
            label = label[1:]
        return label[:-2] if label.endswith("()") else label

    memo = {}

    def qname(nid, seen=()):
        if nid in memo:
            return memo[nid]
        n = nodes[nid]
        if is_file_node(n):
            out = module_name(rel_source(n))
        elif nid in parent and parent[nid] not in seen:
            out = qname(parent[nid], seen + (nid,)) + "." + bare(n.get("label", ""))
        else:
            out = module_name(rel_source(n)) + "." + bare(n.get("label", ""))
        memo[nid] = out
        return out

    edges = set()
    for e in graph.get("links", graph.get("edges", [])):
        if e.get("relation") != "calls":
            continue
        s, t = e["source"], e["target"]
        if s not in nodes or t not in nodes or not nodes[t].get("source_file"):
            continue
        callee = qname(t)
        if nodes[t].get("_callable_class"):
            init = next((m for m in methods.get(t, ()) if bare(nodes[m].get("label", "")) == "__init__"), None)
            if init is None:
                continue
            callee = qname(init)
        edges.add((qname(s), callee))
    return edges


def expected_edges(callgraph: dict) -> set:
    return {(caller, callee) for caller, callees in callgraph.items() for callee in callees}


def project_scope(edges: set, modules: set) -> set:
    def local(name):
        if name.startswith("<") or "<lambda" in name:
            return False
        return any(name == m or name.startswith(m + ".") for m in modules)
    return {(a, b) for a, b in edges if local(a) and local(b)}


def run_impl(spec: str, snippet: Path, work: Path) -> dict:
    exe, _, extra = spec.partition(":")
    out = work / "graphify-out"
    env = {**os.environ, "GRAPHIFY_OUT": str(out), "GRAPHIFY_NO_TIPS": "1", "PYTHONHASHSEED": "0"}
    for key in list(env):
        if any(s in key for s in ("API_KEY", "_TOKEN", "SECRET")):
            env.pop(key)
    argv = ([exe, "-m", "graphify"] if Path(exe).name.startswith("python") else [exe])
    argv += ["extract", str(work), "--code-only", "--force"] + ([extra] if extra else [])
    res = subprocess.run(argv, cwd=work, env=env, capture_output=True, text=True, timeout=300)
    graph = out / "graph.json"
    if res.returncode != 0 or not graph.exists():
        raise RuntimeError(f"{spec} failed on {snippet}: {res.stderr[-800:]}")
    return json.loads(graph.read_text())


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--snippets", required=True, type=Path)
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument("--impl", action="append", required=True, help="name=executable[:extra-flag]")
    args = ap.parse_args()
    impls = {}
    for item in args.impl:
        name, spec = item.split("=", 1)
        exe, sep, extra = spec.partition(":")
        # Snippets run from a temporary directory, so pin relative executables
        # (without following a venv's interpreter symlink out of the venv).
        if Path(exe).exists():
            exe = os.path.abspath(exe)
        impls[name] = exe + sep + extra
    cases = sorted(p.parent for p in args.snippets.rglob("callgraph.json"))
    args.out.mkdir(parents=True, exist_ok=True)
    totals = {name: {scope: defaultdict(int) for scope in ("function", "project", "all")} for name in impls}
    per_category = {name: defaultdict(lambda: defaultdict(int)) for name in impls}
    rows = []
    for case in cases:
        rel = case.relative_to(args.snippets).as_posix()
        category = rel.split("/")[0]
        expected = expected_edges(json.loads((case / "callgraph.json").read_text()))
        py_files = [p.relative_to(case).as_posix() for p in case.rglob("*.py")]
        modules = {module_name(f) for f in py_files}
        # Parent packages count as local too (`nested` for `nested/nested.py`).
        modules |= {".".join(m.split(".")[:i]) for m in modules for i in range(1, len(m.split(".")))}
        row = {"case": rel, "expected": sorted(map(list, expected))}
        for name, spec in impls.items():
            with tempfile.TemporaryDirectory() as tmp:
                work = Path(tmp) / "snippet"
                shutil.copytree(case, work, ignore=shutil.ignore_patterns("callgraph.json", "README.md"))
                predicted = qualified_edges(run_impl(spec, case, work), work)
            row[name] = sorted(map(list, predicted))
            functional = lambda edges: {(a, b) for a, b in project_scope(edges, modules) if a not in modules}
            for scope, exp, pred in (("all", expected, predicted),
                                     ("project", project_scope(expected, modules), project_scope(predicted, modules)),
                                     ("function", functional(expected), functional(predicted))):
                t = totals[name][scope]
                t["tp"] += len(exp & pred)
                t["fp"] += len(pred - exp)
                t["fn"] += len(exp - pred)
                t["cases"] += 1
                t["exact"] += int(exp == pred)
                if scope == "function":
                    c = per_category[name][category]
                    c["tp"] += len(exp & pred)
                    c["fp"] += len(pred - exp)
                    c["fn"] += len(exp - pred)
        rows.append(row)

    def score(t):
        p = t["tp"] / (t["tp"] + t["fp"]) if t["tp"] + t["fp"] else 1.0
        r = t["tp"] / (t["tp"] + t["fn"]) if t["tp"] + t["fn"] else 1.0
        f1 = 2 * p * r / (p + r) if p + r else 0.0
        return {**t, "precision": round(p, 4), "recall": round(r, 4), "f1": round(f1, 4)}

    summary = {
        "benchmark": "PyCG micro-benchmark (vitsalis/PyCG, Apache-2.0), snippets with callgraph.json",
        "cases": len(cases),
        "implementations": impls,
        "totals": {n: {s: score(t) for s, t in by.items()} for n, by in totals.items()},
        "function_by_category": {n: {c: score(t) for c, t in sorted(cats.items())} for n, cats in per_category.items()},
    }
    (args.out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    (args.out / "cases.json").write_text(json.dumps(rows, indent=1) + "\n")
    print(json.dumps(summary["totals"], indent=2))


if __name__ == "__main__":
    sys.exit(main())
