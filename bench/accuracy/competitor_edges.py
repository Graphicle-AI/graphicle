#!/usr/bin/env python3
"""Export a competitor's call edges for oracle_eval.py (--edges NAME=FILE).

  competitor_edges.py codegraph REPO/.codegraph/codegraph.db OUT.json
  competitor_edges.py cbm CACHE/<project>.db OUT.json

Output: {"nodes": [[file, name, line]], "edges": [[caller, callee, call_line]]}
with caller/callee as [file, name, line] (1-based lines, repo-relative files).

codegraph (colbymchenry/codegraph): `calls` and `instantiates` edges between
function, method and class nodes. codebase-memory-mcp (DeusData): `CALLS`
edges between Function, Method and Class nodes (its `CALL_REFERENCE` edges are
function references, not calls, and are left out).
"""
import json
import sqlite3
import sys


def codegraph(db):
    c = sqlite3.connect(db)
    kinds = ("function", "method", "class")
    nodes = {nid: (f, n, l) for nid, f, n, l, k in c.execute(
        "select id, file_path, name, start_line, kind from nodes") if k in kinds}
    edges = []
    for s, t, line in c.execute(
            "select source, target, line from edges where kind in ('calls', 'instantiates')"):
        if s in nodes and t in nodes:
            edges.append([list(nodes[s]), list(nodes[t]), line or 0])
    return {"nodes": [list(v) for v in nodes.values()], "edges": edges}


def cbm(db):
    c = sqlite3.connect(db)
    labels = ("Function", "Method", "Class")
    nodes = {nid: (f, n, l) for nid, f, n, l, lab in c.execute(
        "select id, file_path, name, start_line, label from nodes") if lab in labels}
    edges = []
    for s, t, props in c.execute("select source_id, target_id, properties from edges where type = 'CALLS'"):
        if s in nodes and t in nodes:
            try:
                line = json.loads(props or "{}").get("line", 0)
            except ValueError:
                line = 0
            edges.append([list(nodes[s]), list(nodes[t]), line])
    return {"nodes": [list(v) for v in nodes.values()], "edges": edges}


def main():
    tool, db, out = sys.argv[1:4]
    data = {"codegraph": codegraph, "cbm": cbm}[tool](db)
    with open(out, "w") as f:
        json.dump(data, f)
    print(f"{tool}: {len(data['nodes'])} nodes, {len(data['edges'])} call edges")


if __name__ == "__main__":
    main()
