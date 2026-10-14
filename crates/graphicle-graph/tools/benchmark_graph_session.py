#!/usr/bin/env python3
"""Measure inline graph payloads against resident graphs (`graph.open` + `graph_ref`).

One native worker stays alive. For every generated graph the same query is
timed as (a) a request carrying the whole node-link payload, which is what the
pinned Python and the qualified JSON boundary do on every call, and (b) a
request naming a graph opened once. Results are checked equal on every call,
including the one-off cost of `graph.open`. Timings are wall-clock request
round trips through the JSONL boundary, so (b) includes JSON encoding of the
result and the pipe. No live service or model is used.
"""
import argparse
import datetime
import json
import platform
import statistics
import subprocess
import sys
from pathlib import Path

from benchmark_optimizations import finish, measure
from benchmark_port import graph
from qualification import ROOT, sha, validate


def workloads():
    for size in [2000, 20000, 40000]:
        yield f"sparse_graph_{size}", graph(size), "links", [
            ("affected.hits", {"seed": f"n{size-1}", "depth": 4}),
            ("affected.format", {"query": f"n{size//2}", "depth": 2, "root": str(ROOT)}),
            ("affected.resolve", {"query": f"n{size//3}", "root": str(ROOT)}),
        ]
    for size in [20000]:
        data = graph(size)
        data["links"] = [
            {"source": "n0", "target": f"n{i}", "relation": "calls", "confidence": "EXTRACTED"}
            for i in range(1, size)
        ]
        yield f"hub_graph_{size}", data, "links", [
            ("affected.hits", {"seed": f"n{size-1}", "depth": 4}),
        ]
    for size in [2000, 20000]:
        data = graph(size)
        data["edges"] = data.pop("links")
        yield f"cluster_graph_{size}", data, "edges", [
            ("cluster.partition", {"backend": "leiden", "resolution": 1.0}),
        ]


def encode(value):
    return json.dumps(value, separators=(",", ":")) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--manifest", type=Path, help="Qualification manifest binding the binary")
    parser.add_argument("--samples", type=int, default=12)
    parser.add_argument("--record", default="graph-session")
    args = parser.parse_args()
    args.binary = args.binary.resolve()
    args.manifest = args.manifest.resolve() if args.manifest else None
    qualification = validate(args.manifest, args.binary) if args.manifest else None
    process = subprocess.Popen([str(args.binary)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    rows = []
    try:
        for name, data, edges_key, queries in workloads():
            open_request = encode({"operation": "graph.open", "graph": data, "edges_key": edges_key, "name": name})
            open_ms = []
            for _ in range(2):
                measure(process, open_request)
            for _ in range(args.samples):
                elapsed, _ = measure(process, open_request)
                open_ms.append(elapsed)
            for operation, fields in queries:
                inline = encode({"operation": operation, "graph": data, **fields})
                by_ref = encode({"operation": operation, "graph_ref": name, **fields})
                for _ in range(2):
                    measure(process, inline)
                    measure(process, by_ref)
                inline_ms, ref_ms = [], []
                for _ in range(args.samples):
                    elapsed, expected = measure(process, inline)
                    inline_ms.append(elapsed)
                    elapsed, actual = measure(process, by_ref)
                    ref_ms.append(elapsed)
                    if actual != expected:
                        raise SystemExit(json.dumps({"passed": False, "workload": name, "operation": operation}))
                row = {
                    "workload": name, "operation": operation,
                    "nodes": len(data["nodes"]), "edges": len(data[edges_key]),
                    "payload_bytes": len(inline.encode()),
                    "inline_median_ms": round(statistics.median(inline_ms), 3),
                    "graph_ref_median_ms": round(statistics.median(ref_ms), 3),
                    "graph_open_median_ms": round(statistics.median(open_ms), 3),
                    "speedup": round(statistics.median(inline_ms) / statistics.median(ref_ms), 1),
                    "inline_ms": [round(v, 3) for v in inline_ms],
                    "graph_ref_ms": [round(v, 3) for v in ref_ms],
                }
                rows.append(row)
                print(json.dumps({k: v for k, v in row.items() if not k.endswith("_ms") or k.endswith("median_ms")}), flush=True)
            measure(process, encode({"operation": "graph.close", "graph_ref": name}))
    finally:
        resources = finish(process)
    report = {
        "recorded_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "host": {"platform": platform.platform(), "machine": platform.machine(), "python": platform.python_version()},
        "binary": {"path": str(args.binary), "sha256": sha(args.binary),
                   "qualification_manifest": str(args.manifest.relative_to(ROOT)) if args.manifest else None,
                   "source_set_sha256": qualification["source_set_sha256"] if qualification else None},
        "samples_per_measurement": args.samples,
        "method": "One persistent worker; two untimed warmups per request; inline and graph_ref requests alternate; results compared on every sample.",
        "worker_resources": resources,
        "rows": rows,
    }
    path = ROOT / "migration/benchmarks" / f"{args.record}.json"
    path.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"passed": True, "report": str(path)}), flush=True)


if __name__ == "__main__":
    sys.exit(main())
