#!/usr/bin/env python3
"""Score `--routes` output against a route oracle (fastapi_routes.py).

A predicted route is [handler file, handler name, handler line, METHOD,
path] read from `route` nodes and their `handles` edges. It matches an
oracle route with the same file, name, method and path and a handler line
within 3. Only files under the oracle's directory are scored. Requests
(`http_calls` edges) are reported with how many target a route in the
caller's own file or in a file the caller's file imports.

Usage: routes_eval.py GRAPH.json ORACLE.json REPO SUBDIR > summary.json
"""
import json, os, sys
from collections import Counter


def main():
    graph, oracle, repo, sub = sys.argv[1], sys.argv[2], os.path.realpath(sys.argv[3]), sys.argv[4]
    g = json.load(open(graph))
    nodes = {n["id"]: n for n in g["nodes"]}
    links = g.get("links", g.get("edges", []))

    def rel(f):
        f = os.path.realpath(f) if os.path.isabs(f) else os.path.realpath(os.path.join(repo, f))
        return os.path.relpath(f, repo)

    def line(n):
        loc = str(n.get("source_location") or "")
        return int(loc[1:].split("-")[0]) if loc.startswith("L") and loc[1:].split("-")[0].isdigit() else -1

    predicted = set()
    routes = {i for i, n in nodes.items() if n.get("node_kind") == "route"}
    handled = set()
    for e in links:
        if e.get("relation") != "handles":
            continue
        a, b = e.get("_src", e["source"]), e.get("_tgt", e["target"])
        route, handler = (b, a) if b in routes else (a, b)
        if route not in routes or handler not in nodes:
            continue
        handled.add(route)
        r, h = nodes[route], nodes[handler]
        name = (h.get("label") or "").lstrip(".").removesuffix("()")
        predicted.add((rel(h["source_file"]), name, line(h), r["route_method"], r["route_path"]))
    oracle_data = json.load(open(oracle))
    truth = {tuple(x) for x in oracle_data["routes"] if x[0].startswith(sub)}
    # Modules the oracle could not import cannot confirm or deny a route.
    unimported = {s[0] for s in oracle_data.get("skipped", [])}
    scored = {p for p in predicted if p[0].startswith(sub) and p[0] not in unimported}

    def hit(p, pool):
        return any(p[0] == t[0] and p[1] == t[1] and abs(p[2] - t[2]) <= 3 and p[3] == t[3] and p[4] == t[4] for t in pool)

    tp = sum(hit(p, truth) for p in scored)
    found = sum(hit(t, scored) for t in truth)
    misses = sorted(t for t in truth if not hit(t, scored))
    wrong = sorted(p for p in scored if not hit(p, truth))
    file_of = {i: rel(n["source_file"]) for i, n in nodes.items() if n.get("source_file")}
    imports = {}
    for e in links:
        if e.get("relation") in ("imports", "imports_from"):
            a, b = e.get("_src", e["source"]), e.get("_tgt", e["target"])
            if a in file_of and b in file_of:
                imports.setdefault(file_of[a], set()).add(file_of[b])
                imports.setdefault(file_of[b], set()).add(file_of[a])  # undirected graphs
    calls = [e for e in links if e.get("relation") == "http_calls"]
    consistent = 0
    for e in calls:
        a, b = e.get("_src", e["source"]), e.get("_tgt", e["target"])
        if b not in routes and a in routes:
            a, b = b, a
        fa, fb = file_of.get(a), file_of.get(b)
        consistent += fa == fb or fb in imports.get(fa, set())
    summary = {
        "route_nodes": len(routes), "routes_with_handler": len(handled),
        "scored_under": sub, "predicted": len(scored), "oracle": len(truth),
        "precision": round(tp / len(scored), 4) if scored else None,
        "recall": round(found / len(truth), 4) if truth else None,
        "methods": dict(Counter(p[3] for p in scored)),
        "http_calls": len(calls),
        "http_calls_to_own_or_imported_file": consistent,
        "missed_sample": misses[:15], "wrong_sample": wrong[:15],
    }
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
