#!/usr/bin/env python3
"""Call-graph accuracy on real repositories against independent oracles.

Oracles
  scip   Call edges read from a SCIP index written by a compiler-grade indexer
         (scip-python = Pyright, scip-typescript = the TypeScript checker,
         scip-go, rust-analyzer). An edge is a reference occurrence whose
         source text is a call (`name(`, `name<T>(`, `new Name(`) of an
         in-repository function, method or class, attributed to the innermost
         enclosing function definition. Constructors map to their class.
  trace  Python call edges observed while running the test suite
         (trace_calls.py). Every trace edge is a real call; the trace cannot
         show calls the tests never made, so only recall is reported for it.

Tools are scored on the edges whose endpoints are definitions the oracle
knows (graph nodes are matched to definitions by file, name and line, within
3 lines). Precision is against the SCIP oracle, so a correct edge the type
checker cannot see (duck typing, dynamic dispatch) counts as a false positive:
treat it as agreement with the type checker, a lower bound on precision.

Usage
  oracle_eval.py --repo DIR --out DIR --scip INDEX [--trace calls.json]
      --impl name=EXE[:FLAG][@VAR=VALUE] ...     (graph.json producers)
      [--codegraph BIN] [--cbm BIN]              (competitors, optional)
"""
import argparse, bisect, json, os, re, shutil, subprocess, sys, tempfile
from collections import defaultdict
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import scip_pb  # noqa: E402

LINE_SLACK = 3
JS_SUFFIXES = (".ts", ".tsx", ".js", ".jsx", ".mts", ".cts", ".mjs", ".cjs")
IDENT = re.compile(r"[A-Za-z_$][\w$]*")
# `<Name` opening a JSX element: the `<` starts an expression (line start, or
# after an operator, bracket or `return`), unlike a type argument `Foo<Name>`.
JSX_OPEN = re.compile(r"(?:^|[(\[{,;:?=&|!>]|\breturn)\s*<$")


def is_call(line, end, following=()):
    """Is the reference ending at `end` of `line` called: `name(` or
    `name<T>(`? Type arguments may run onto the `following` lines."""
    rest = line[end:].lstrip()
    if rest.startswith("("):
        return True
    if rest.startswith("<"):
        depth = 0
        texts = (rest, *following[:40])
        for k, text in enumerate(texts):
            for i, ch in enumerate(text):
                if ch == "<":
                    depth += 1
                elif ch == ">":
                    depth -= 1
                    if depth == 0:
                        tail = text[i + 1:].lstrip()
                        if tail:
                            return tail.startswith("(")
                        # `>` ends the line: the call's `(` may start the next.
                        nxt = next((t.lstrip() for t in texts[k + 1:] if t.strip()), "")
                        return nxt.startswith("(")
                elif ch in ";{}":
                    return False
    return False


class Definitions:
    """In-repository callable definitions of a SCIP index, with extents."""

    def __init__(self, index, repo, ts_extents=None):
        root, docs = scip_pb.read_index(index)
        self.base = Path(root) if root else repo
        self.repo = repo
        self.docs = docs
        self.defs = {}           # id -> dict(file, name, line, kind, extent)
        self.by_symbol = {}      # symbol -> id
        self.by_file_name = defaultdict(list)
        self.lines = {}
        for doc in docs:
            rel = self.rel(doc["path"])
            for occ in doc["occurrences"]:
                if not occ["roles"] & scip_pb.DEFINITION or occ["symbol"].startswith("local "):
                    continue
                desc = scip_pb.last_descriptor(occ["symbol"])
                if not desc:
                    continue
                name, suffix = desc
                kind = {"().": "function", "#": "class", ".": "term"}.get(suffix)
                if kind is None or name == "<constructor>":
                    continue
                if occ["symbol"] in self.by_symbol:
                    # Overloads repeat the definition; the last one is the
                    # implementation, so every definition line is kept.
                    d = self.defs[self.by_symbol[occ["symbol"]]]
                    d["lines"].append(occ["range"][0])
                    if occ["enclosing"]:
                        d["extent"] = occ["enclosing"]
                    continue
                did = len(self.defs)
                self.defs[did] = {"file": rel, "name": name, "line": occ["range"][0],
                                  "lines": [occ["range"][0]],
                                  "kind": kind, "extent": occ["enclosing"], "symbol": occ["symbol"]}
                self.by_symbol[occ["symbol"]] = did
                # scip-python names definitions of a src/ layout `src.pkg.mod`
                # but references to them `pkg.mod` (the import path).
                if rel.startswith("src/"):
                    for old, new in (("`src.", "`"), (" src/", " ")):
                        alias = occ["symbol"].replace(old, new, 1)
                        if alias != occ["symbol"]:
                            self.by_symbol.setdefault(alias, did)
                self.by_file_name[(rel, name)].append(did)
        # Python: every `def`/`class` statement from the source's AST, keyed
        # by qualified name. scip-python records one definition per symbol
        # (the first `@overload`), so overload lines, the implementation's
        # extent and traced code objects are matched through this table.
        self.by_qual = {}
        py_files = {d["file"] for d in self.defs.values() if d["file"].endswith(".py")}
        for rel in py_files:
            for qual, def_line, first, last in python_defs(repo / rel):
                did = next((i for i in self.by_file_name.get((rel, qual.rsplit(".", 1)[-1]), ())
                            if scip_pb.qualname(self.defs[i]["symbol"]) == qual), None)
                if did is None:
                    continue
                d = self.defs[did]
                self.by_qual[(rel, qual)] = did
                if def_line not in d["lines"]:
                    d["lines"].append(def_line)
                # The last statement is the implementation of an overload set.
                if def_line >= max(d["lines"]):
                    d["extent"] = (first, 0, last, 10**6)
                    d["body_line"] = def_line
        # JS/TS: the TypeScript parser gives the declaration kind and the
        # extents SCIP lacks (function-valued variables and properties). A
        # SCIP type symbol (`#`) is a class only if the parser saw a class
        # declaration there; interfaces and type aliases are not callable.
        if ts_extents is not None:
            seen = {}
            for rel, items in ts_extents.items():
                for name, name_line, sl, sc, el, ec, what in (i[:7] for i in items):
                    seen[(rel, name, name_line)] = (what, (sl, sc, el, ec))
            for d in self.defs.values():
                hit = seen.get((d["file"], d["name"], d["line"]))
                if hit and hit[0] == "function":
                    d["kind"] = "function"
                    d["extent"] = d["extent"] or hit[1]
                elif hit and hit[0] == "class":
                    d["kind"] = "class"
                elif d["kind"] in ("class", "function") and not hit:
                    d["kind"] = "type" if d["kind"] == "class" else d["kind"]
                elif hit:
                    d["kind"] = "term"
                    d["extent"] = d["extent"] or hit[1]
        # Containers a call is attributed to: functions and methods, then any
        # other definition with an extent (classes, initialized variables),
        # at the innermost level a tool has a node for (`edges_for`).
        self.containers = defaultdict(list)
        for did, d in self.defs.items():
            if d["extent"] and d["kind"] in ("function", "class", "term"):
                self.containers[d["file"]].append((d["extent"], did))

    def rel(self, path):
        full = (self.base / path).resolve()
        try:
            return full.relative_to(self.repo.resolve()).as_posix()
        except ValueError:
            return path

    def text(self, rel):
        if rel not in self.lines:
            try:
                self.lines[rel] = (self.repo / rel).read_text(errors="replace").split("\n")
            except OSError:
                self.lines[rel] = []
        return self.lines[rel]

    def chain(self, rel, pos):
        """Definitions whose body contains `pos`, innermost first. A call on
        a decorator line (before the definition's name) runs in the enclosing
        scope, so it does not belong to the decorated definition."""
        hits = []
        for (sl, sc, el, ec), did in self.containers.get(rel, ()):
            d = self.defs[did]
            if (sl, sc) <= pos <= (el, ec) and pos[0] >= d.get("body_line", d["line"]):
                hits.append(((el - sl, ec - sc if el == sl else ec), did))
        hits.sort()
        return tuple(did for _, did in hits)

    def oracle_sites(self):
        """(sites, disputed). A site is (enclosing chain, callee) for a call
        (`name(`, `name<T>(`, `new Name(`) or JSX render (`<Name`) of an
        in-repository function, method or class. Disputed sites are the
        (file, line, name) call sites whose SCIP symbol names something other
        than the identifier written there (an indexer error: scip-python 0.6.6
        gives every name of a `from m import *` re-export read as `pkg.Name`
        the first name's symbol). Nobody is scored at disputed sites."""
        sites, disputed = set(), set()
        for doc in self.docs:
            rel = self.rel(doc["path"])
            lines = self.text(rel)
            for occ in doc["occurrences"]:
                if occ["roles"] & (scip_pb.DEFINITION | scip_pb.IMPORT):
                    continue
                symbol = occ["symbol"]
                desc = scip_pb.last_descriptor(symbol)
                if not desc:
                    continue
                target = self.by_symbol.get(symbol)
                if desc[0] == "<constructor>":
                    parent = scip_pb.parent_symbol(symbol)
                    target = self.by_symbol.get(parent) if parent else None
                if target is None or self.defs[target]["kind"] not in ("function", "class"):
                    continue
                sl, sc, el, ec = occ["range"]
                if sl != el or sl >= len(lines):
                    continue
                line = lines[sl]
                a = scip_pb.utf16_to_byte(line, sc) if doc["utf16"] else sc
                b = scip_pb.utf16_to_byte(line, ec) if doc["utf16"] else ec
                jsx = rel.endswith((".tsx", ".jsx", ".js")) and JSX_OPEN.search(line[:a]) is not None
                if not (jsx or is_call(line, b, lines[sl + 1:sl + 41])):
                    continue
                written = line[a:b]
                # scip-python 0.6.6 can give a reference the wrong symbol (see
                # above). Elsewhere a different identifier is an import alias
                # (`import { f as g }`, Rust `use f as g`), a real call of f.
                if (desc[0] != "<constructor>" and written != self.defs[target]["name"]
                        and rel.endswith((".py", ".pyi"))):
                    if IDENT.fullmatch(written):
                        disputed.add((rel, sl, written))
                    continue
                if implicit(self.defs[target]["name"]):
                    continue
                chain = self.chain(rel, (sl, sc))
                if chain:
                    sites.add((chain, target))
        return sites, disputed

    def oracle_edges(self):
        sites, disputed = self.oracle_sites()
        return {(c[0], t) for c, t in sites if c[0] != t}, disputed

    def match(self, rel, name, line, forward=0):
        """Definition id for a node/trace entry: same file and name, nearest
        definition line (0-based) within the slack; `forward` extra lines
        allowed after it (decorators precede the name)."""
        best = None
        for did in self.by_file_name.get((rel, name), ()):
            for def_line in self.defs[did]["lines"]:
                delta = def_line - line
                if -LINE_SLACK <= delta <= LINE_SLACK + forward:
                    key = (abs(delta), did)
                    if best is None or key < best:
                        best = key
        return best[1] if best else None


def edges_for(sites, ids):
    """Oracle edges at a tool's granularity: each call attributed to the
    innermost enclosing definition the tool has a node for."""
    out = set()
    for chain, target in sites:
        caller = next((c for c in chain if c in ids), None)
        if caller is not None and caller != target:
            out.add((caller, target))
    return out


def python_defs(path):
    """(qualname, def line, first line, last line) of every function and class
    statement (0-based lines; the first line includes decorators)."""
    import ast
    try:
        tree = ast.parse(path.read_text(errors="replace"))
    except (SyntaxError, ValueError, OSError):
        return []
    out = []

    def visit(node, prefix):
        for child in ast.iter_child_nodes(node):
            if isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
                qual = f"{prefix}.{child.name}" if prefix else child.name
                first = min([child.lineno] + [d.lineno for d in child.decorator_list]) - 1
                out.append((qual, child.lineno - 1, first, (child.end_lineno or child.lineno) - 1))
                visit(child, qual)
            else:
                visit(child, prefix)

    visit(tree, "")
    return out


def bare(label):
    label = label or ""
    if label.startswith("."):
        label = label[1:]
    return label[:-2] if label.endswith("()") else label


def implicit(name):
    """Dunder methods other than constructors run through protocols (`for`,
    `with`, operators), not call syntax, so call-syntax oracles cannot see
    them. Scoring covers explicit calls only."""
    return name.startswith("__") and name.endswith("__") and name != "__init__"


def edge_label(e):
    """How a graph edge was found: its precise-pass `resolution`, receiver
    evidence, or Graphify's own confidence tier."""
    if e.get("resolution"):
        return str(e["resolution"])
    if e.get("receiver_evidence"):
        return "receiver:" + str(e["receiver_evidence"])
    return "graphify:" + str(e.get("confidence") or "?")


def graph_edges(graph, defs, repo, disputed=frozenset(), meta=None):
    """(edges, node definition ids, edges skipped at disputed sites). With
    `meta` (a dict), also records each scored edge's labels (`edge_label`)
    and call sites: meta[(caller, callee)] = [(label, file, line), ...]."""
    nodes = {n["id"]: n for n in graph["nodes"]}
    mapped, node_ids = {}, set()
    skipped = 0

    def rel_file(sf):
        p = Path(sf)
        if p.is_absolute():
            try:
                return p.relative_to(repo).as_posix()
            except ValueError:
                pass
        return sf

    def node_def(nid):
        if nid in mapped:
            return mapped[nid]
        n = nodes.get(nid)
        out = None
        if n:
            sf = rel_file(n.get("source_file") or "")
            loc = str(n.get("source_location") or "")
            m = re.match(r"L(\d+)", loc)
            if m:
                out = defs.match(sf, bare(n.get("label")), int(m.group(1)) - 1)
        mapped[nid] = out
        return out

    for nid in nodes:
        d = node_def(nid)
        if d is not None:
            node_ids.add(d)
    edges = set()
    for e in graph.get("links", graph.get("edges", [])):
        if e.get("relation") != "calls":
            continue
        src, tgt = e.get("_src", e["source"]), e.get("_tgt", e["target"])
        a, b = node_def(src), node_def(tgt)
        if a is None or b is None or a == b or implicit(defs.defs[b]["name"]):
            continue
        m = re.match(r"L(\d+)", str(e.get("source_location") or ""))
        site = (rel_file(e.get("source_file") or ""), int(m.group(1)) - 1 if m else -1,
                defs.defs[b]["name"])
        if site in disputed:
            skipped += 1
            continue
        edges.add((a, b))
        if meta is not None:
            meta.setdefault((a, b), []).append((edge_label(e), site[0], site[1] + 1, e.get("evidence") or "none"))
    return edges, node_ids, skipped


def trace_edges(path, defs):
    edges, unmapped = set(), 0

    def resolve(f, q, line):
        q = q.replace(".<locals>", "")
        did = defs.by_qual.get((f, q))
        if did is None:
            # co_firstlineno is the first decorator's line; the name follows it.
            did = defs.match(f, q.rsplit(".", 1)[-1], line - 1, forward=8)
        return did

    for (cf, cq, cl), (tf, tq, tl) in json.load(open(path)):
        cname, tname = cq.rsplit(".", 1)[-1], tq.rsplit(".", 1)[-1]
        if tname.startswith("<") or cname.startswith("<") or implicit(tname):
            continue
        a, b = resolve(cf, cq, cl), resolve(tf, tq, tl)
        if a is None or b is None:
            unmapped += 1
        elif a != b:
            edges.add((a, b))
    return edges, unmapped


def run_graphify(spec, repo, work):
    spec, _, envs = spec.partition("@")
    exe, _, flag = spec.partition(":")
    # Tools run from the repository: pin relative executables first (without
    # following a venv interpreter's symlink out of the venv).
    if Path(exe).exists():
        exe = os.path.abspath(exe)
    out = work / "graphify-out"
    env = {k: v for k, v in os.environ.items() if not re.search(r"API_KEY|_TOKEN|SECRET", k)}
    env.update(GRAPHIFY_OUT=str(out), GRAPHIFY_NO_TIPS="1", PYTHONHASHSEED="0")
    env.pop("GRAPHIFY_SCIP", None)
    for item in filter(None, envs.split(",")):
        k, _, v = item.partition("=")
        env[k] = v
    argv = [exe, "-m", "graphify"] if Path(exe).name.startswith("python") else [exe]
    argv += ["extract", str(repo), "--code-only", "--force"] + ([flag] if flag else [])
    res = subprocess.run(argv, cwd=repo, env=env, capture_output=True, text=True, timeout=3600)
    if res.returncode != 0:
        raise RuntimeError(f"{spec} failed: {res.stderr[-1500:]}")
    return json.loads((out / "graph.json").read_text())


def score(pred, oracle):
    tp = len(pred & oracle)
    p = tp / len(pred) if pred else 1.0
    r = tp / len(oracle) if oracle else 1.0
    f1 = 2 * p * r / (p + r) if p + r else 0.0
    return {"tp": tp, "fp": len(pred - oracle), "fn": len(oracle - pred),
            "precision": round(p, 4), "recall": round(r, 4), "f1": round(f1, 4)}


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--repo", required=True, type=Path)
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument("--scip", required=True, type=Path)
    ap.add_argument("--trace", type=Path)
    ap.add_argument("--impl", action="append", default=[], help="name=EXE[:FLAG][@VAR=VALUE,...]")
    ap.add_argument("--edges", action="append", default=[], help="name=edges.json from a competitor adapter")
    ap.add_argument("--ts-extents", type=Path, help="JSON from ts_extents.js")
    ap.add_argument("--dump", action="store_true",
                    help="write <out>/<impl>-edges.json: every scored edge with its labels and whether an oracle confirms it")
    args = ap.parse_args()
    repo = args.repo.resolve()
    extents = json.loads(args.ts_extents.read_text()) if args.ts_extents else None
    defs = Definitions(args.scip, repo, extents)
    sites, disputed = defs.oracle_sites()
    oracle = {(c[0], t) for c, t in sites if c[0] != t}
    trace, trace_unmapped = (trace_edges(args.trace, defs) if args.trace else (set(), 0))
    args.out.mkdir(parents=True, exist_ok=True)
    results = {}
    predicted = {}
    metas = {}
    for item in args.impl:
        name, spec = item.split("=", 1)
        with tempfile.TemporaryDirectory() as tmp:
            graph = run_graphify(spec, repo, Path(tmp))
        metas[name] = {}
        predicted[name] = graph_edges(graph, defs, repo, disputed, metas[name])
    for item in args.edges:
        name, path = item.split("=", 1)
        rows = json.loads(Path(path).read_text())
        edges, ids, skipped = set(), set(), 0
        for (cf, cn, cl), (tf, tn, tl), *site in rows["edges"]:
            # Other tools may start a definition at its first decorator.
            a, b = defs.match(cf, cn, cl - 1, forward=8), defs.match(tf, tn, tl - 1, forward=8)
            if a is None or b is None or a == b or implicit(defs.defs[b]["name"]):
                continue
            if site and (cf, site[0] - 1, defs.defs[b]["name"]) in disputed:
                skipped += 1
                continue
            edges.add((a, b))
        for f, n, l in rows.get("nodes", []):
            d = defs.match(f, n, l - 1, forward=8)
            if d is not None:
                ids.add(d)
        predicted[name] = (edges, ids, skipped)
    for name, (edges, ids, skipped) in predicted.items():
        own = edges_for(sites, ids)
        reachable = {e for e in own if e[1] in ids}
        entry = {"edges_scored": len(edges), "edges_skipped_at_disputed_sites": skipped,
                 "oracle_edges": len(own),
                 "scip": score(edges, own),
                 "scip_recall_callee_is_node": score(edges, reachable)["recall"],
                 "oracle_edges_callee_is_node": len(reachable)}
        if args.trace:
            t = score(edges, trace)
            entry["trace"] = {"tp": t["tp"], "fn": t["fn"], "recall": t["recall"]}
            treach = {e for e in trace if e[0] in ids and e[1] in ids}
            entry["trace_recall_on_own_nodes"] = score(edges, treach)["recall"]
            # An edge is confirmed when the type checker or the test run shows
            # it; unconfirmed edges may still be real (untested, dynamic).
            confirmed = edges & (own | trace)
            entry["confirmed_precision"] = round(len(confirmed) / len(edges), 4) if edges else 1.0
            entry["unconfirmed_edges"] = len(edges) - len(confirmed)
        meta = metas.get(name)
        if meta:
            # Precision of each way of finding an edge (an edge found two
            # ways counts under both), and at or above each evidence level.
            by = defaultdict(lambda: {"edges": 0, "scip_tp": 0, "confirmed": 0})
            levels = defaultdict(lambda: {"edges": 0, "scip_tp": 0, "confirmed": 0})
            order = ["none", "name", "binding", "type", "indexer", "runtime"]
            for pair in edges:
                for label in sorted({m[0] for m in meta.get(pair, [])}):
                    row = by[label]
                    row["edges"] += 1
                    row["scip_tp"] += pair in own
                    row["confirmed"] += pair in own or pair in trace
                best = max((order.index(m[3]) if m[3] in order else 0 for m in meta.get(pair, [])), default=0)
                for lvl in order[1:best + 1] or ["none"]:
                    row = levels[lvl]
                    row["edges"] += 1
                    row["scip_tp"] += pair in own
                    row["confirmed"] += pair in own or pair in trace
            for table in (by, levels):
                for row in table.values():
                    row["scip_precision"] = round(row["scip_tp"] / row["edges"], 4)
                    row["scip_recall"] = round(row["scip_tp"] / len(own), 4) if own else None
                    if args.trace:
                        row["confirmed_precision"] = round(row["confirmed"] / row["edges"], 4)
                    else:
                        del row["confirmed"]
            entry["by_label"] = dict(sorted(by.items()))
            if any(lvl != "none" for lvl in levels):
                # "at_least" rows: the edges an agent keeps with --min-evidence LEVEL.
                entry["at_least_evidence"] = {lvl: levels[lvl] for lvl in order if lvl in levels}
            if args.dump:
                def describe(d):
                    x = defs.defs[d]
                    return [x["file"], x["name"], x.get("line", -1) + 1]
                rows = [{"caller": describe(a), "callee": describe(b),
                         "labels": sorted({m[0] for m in meta.get((a, b), [])}),
                         "sites": sorted({(m[1], m[2]) for m in meta.get((a, b), [])}),
                         "scip": (a, b) in own, "trace": (a, b) in trace}
                        for a, b in sorted(edges, key=lambda p: (defs.defs[p[0]]["file"], p))]
                (args.out / f"{name}-edges.json").write_text(json.dumps(rows, indent=0) + "\n")
        results[name] = entry
    summary = {
        "repo": str(repo),
        "scip_index": str(args.scip),
        "definitions": len(defs.defs),
        "oracle_scip_edges": len(oracle),
        "disputed_call_sites": len(disputed),
        "oracle_trace_edges": len(trace),
        "trace_entries_unmapped": trace_unmapped,
        "results": results,
    }
    (args.out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    sys.exit(main())
