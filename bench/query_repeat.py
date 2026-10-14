#!/usr/bin/env python3
"""Repeated-query latency: the first `query` on a fresh graph.json (which
also writes the query snapshot) and the following ones (which read it),
against Python Graphify on the same graph. bench/run.py times only the first.

Usage: query_repeat.py --native BIN --python PY --repos-dir DIR --out FILE [--reps 5]
    [--graph NAME=GRAPH.json ...]

`--graph` also times an existing graph (copied into a fresh output
directory), using the question of the repository NAME starts with.
"""
import argparse, hashlib, json, os, shutil, statistics, subprocess, sys, tempfile, time
from pathlib import Path

QUERIES = {
    "click": "how does command parsing work",
    "httpx": "how are requests sent through the transport",
    "excalidraw": "how is the canvas rendered",
    "fastapi": "how are path operations registered",
    "django": "how are url patterns resolved",
}


def timed(argv, env):
    t0 = time.perf_counter()
    out = subprocess.run(argv, capture_output=True, text=True, env=env)
    assert out.returncode == 0, out.stderr[-500:]
    return time.perf_counter() - t0, out.stdout


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--native", required=True)
    ap.add_argument("--python", required=True)
    ap.add_argument("--repos-dir", type=Path, default=Path("/tmp/gf-bench-repos"))
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--reps", type=int, default=5)
    ap.add_argument("--graph", action="append", default=[], help="NAME=GRAPH.json (existing graph)")
    args = ap.parse_args()
    env = {k: v for k, v in os.environ.items() if not any(s in k for s in ("API_KEY", "_TOKEN", "SECRET"))}
    env["GRAPHIFY_NO_TIPS"] = "1"
    # Never let Python Graphify 0.9.72 refresh the user's installed skills (#1805).
    env["GRAPHIFY_NO_AUTO_REFRESH"] = "1"
    results = {}
    jobs = [(repo, q, None) for repo, q in QUERIES.items()]
    for spec in args.graph:
        name, _, path = spec.partition("=")
        question = next(q for repo, q in QUERIES.items() if name.startswith(repo))
        jobs.append((name, question, Path(path)))
    for repo, q, existing in jobs:
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "graphify-out"
            e = dict(env, GRAPHIFY_OUT=str(out))
            if existing is None:
                timed([args.native, "extract", str(args.repos_dir / repo), "--code-only", "--force"], e)
            else:
                (out / "cache").mkdir(parents=True)
                shutil.copyfile(existing, out / "graph.json")
            graph = str(out / "graph.json")
            first, text = timed([args.native, "query", q, "--graph", graph, "--budget", "2000"], e)
            warm, outputs = [], {text}
            for _ in range(args.reps):
                s, t = timed([args.native, "query", q, "--graph", graph, "--budget", "2000"], e)
                warm.append(s)
                outputs.add(t)
            py = [timed([args.python, "-m", "graphify", "query", q, "--graph", graph, "--budget", "2000"],
                        dict(e, PYTHONHASHSEED="0"))[0] for _ in range(3)]
            results[repo] = {"native_first_s": round(first, 3), "native_warm_median_s": round(statistics.median(warm), 3),
                             "python_median_s": round(statistics.median(py), 3),
                             "native_outputs_identical": len(outputs) == 1,
                             "nodes": len(json.loads(Path(graph).read_text())["nodes"]),
                             "snapshot_bytes": sum(p.stat().st_size for p in (out / "cache" / "query").glob("*.snap"))}
            print(repo, results[repo], file=sys.stderr)
    digest = hashlib.sha256(Path(args.native).read_bytes()).hexdigest()
    args.out.write_text(json.dumps({"native": args.native, "native_sha256": digest, "reps": args.reps,
                                    "results": results}, indent=2) + "\n")


if __name__ == "__main__":
    main()
