#!/usr/bin/env python3
"""Interleave two qualified release binaries and Python on identical workloads.

Each process stays alive across samples. Requests and corpus paths are identical
for every implementation, results are checked on every call, and each workload
has two untimed warmups. Reports include every timing and worker peak RSS.
The baseline manifest binds the old binary; only the candidate source set must
match the current checkout. No live service or model is used.
"""
import argparse
import datetime
import hashlib
import itertools
import json
import os
import platform
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from benchmark_port import canonical, graph
from naming import native_env
from qualification import ROOT, harness_sources, sha, validate


def workloads(root):
    for size in [2000, 20000]:
        yield f"sparse_graph_{size}", {
            "operation": "affected.hits", "graph": graph(size),
            "seed": f"n{size-1}", "depth": 4,
        }
    for size, parallel in [(20000, False), (40000, False), (20000, True)]:
        data = graph(size)
        data["multigraph"] = parallel
        data["links"] = [
            {"source": "n0", "target": f"n{i}", "relation": relation, "confidence": "EXTRACTED"}
            for i in range(1, size)
            for relation in (["calls", "references"] if parallel else ["calls"])
        ]
        yield f"{'parallel_' if parallel else ''}hub_graph_{size}", {
            "operation": "affected.hits", "graph": data,
            "seed": f"n{size-1}", "depth": 4,
        }
    for language, suffix, count, parallel in [("python", "py", 100, False), ("typescript", "ts", 100, False),
                                              ("python", "py", 1000, False), ("python", "py", 1000, True)]:
        corpus = root / f"{language}_{count}_{parallel}"
        corpus.mkdir()
        paths = []
        for i in range(count):
            path = corpus / f"unit_{i}.{suffix}"
            text = (
                f"def normalize_{i}(value):\n    return value.strip()\n\ndef handle_{i}(value):\n    return normalize_{i}(value)\n"
                if language == "python" else
                f"export function normalize_{i}(value: string): string {{ return value.trim(); }}\n"
                f"export function handle_{i}(value: string): string {{ return normalize_{i}(value); }}\n"
            )
            path.write_text(text)
            paths.append(str(path))
        yield f"{language}_extraction_{count}_files{'_4_workers' if parallel else ''}", {
            "operation": "extract", "paths": sorted(paths), "root": str(corpus),
            "parallel": parallel, "use_cache": False,
        }


def measure(process, payload):
    started = time.perf_counter_ns()
    process.stdin.write(payload)
    process.stdin.flush()
    line = process.stdout.readline()
    elapsed = (time.perf_counter_ns() - started) / 1e6
    if not line:
        raise RuntimeError("Benchmark worker closed its output")
    response = json.loads(line)
    if not response.get("ok"):
        raise RuntimeError(response)
    return elapsed, canonical(response["result"])


def finish(process):
    process.stdin.close()
    _, status, usage = os.wait4(process.pid, 0)
    process.returncode = os.waitstatus_to_exitcode(status)
    if process.returncode:
        raise RuntimeError(f"Benchmark worker exited with {process.returncode}")
    return {
        "peak_rss_bytes": usage.ru_maxrss * (1 if sys.platform == "darwin" else 1024),
        "user_cpu_seconds": usage.ru_utime,
        "system_cpu_seconds": usage.ru_stime,
        "scope": "whole persistent worker across all workloads, including warmup",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--baseline-manifest", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--candidate-manifest", type=Path, required=True)
    parser.add_argument("--samples", type=int, default=18,
                        help="Timed samples per workload; multiples of six balance all worker orders")
    parser.add_argument("--record", default="optimization-release")
    parser.add_argument("--workloads", nargs="*", help="Optional exact workload names for a smoke run")
    args = parser.parse_args()
    if args.samples < 3:
        parser.error("Use at least three timed samples")
    baseline = json.loads(args.baseline_manifest.read_text())
    candidate = validate(args.candidate_manifest, args.candidate)
    harness = harness_sources()
    baseline_manifest_sha256 = sha(args.baseline_manifest)
    assert baseline["profile"] == candidate["profile"] == "release"
    assert sha(args.baseline) == baseline["binary_sha256"]
    directory = ROOT / "migration/benchmarks"
    directory.mkdir(parents=True, exist_ok=True)
    names = ["baseline", "optimized", "python"]
    commands = [[str(args.baseline.resolve()), "--jsonl"], [str(args.candidate.resolve()), "--jsonl"],
                [sys.executable, str(ROOT / "tools/benchmark_port.py"), "--worker"]]
    # The same settings for the reference worker (GRAPHIFY_*) and both engine builds (GRAPHICLE_*).
    environment = native_env({**os.environ, "GRAPHIFY_NO_INCREMENTAL_CACHE": "1", "GRAPHIFY_MAX_WORKERS": "4"}, "native")
    rows, processes, logs, resources = [], [], [], {}
    try:
        for name, command in zip(names, commands):
            log = (directory / f"{args.record}-{name}.log").open("w")
            logs.append(log)
            processes.append(subprocess.Popen(command, cwd=ROOT, env=environment, text=True,
                                               stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log))
        with tempfile.TemporaryDirectory(prefix="graphify-compare-") as temporary:
            for name, value in workloads(Path(temporary).resolve()):
                if args.workloads and name not in args.workloads:
                    continue
                payload = json.dumps(value, separators=(",", ":")) + "\n"
                order = itertools.cycle(itertools.permutations(range(len(processes))))
                expected = None
                times = {worker: [] for worker in names}
                for iteration in range(args.samples + 2):
                    for which in next(order):
                        elapsed, result = measure(processes[which], payload)
                        if expected is None:
                            expected = result
                        if result != expected:
                            failure = directory / f"{args.record}-mismatch.json"
                            failure.write_text(json.dumps({"workload": name, "worker": names[which],
                                                           "expected": expected, "actual": result}, indent=2) + "\n")
                            raise AssertionError(f"Result mismatch: {name}, {names[which]}; {failure}")
                        if iteration >= 2:
                            times[names[which]].append(elapsed)
                medians = {worker: statistics.median(values) for worker, values in times.items()}
                row = {"workload": name, "request_bytes": len(payload.encode()), "samples": args.samples,
                       "request_sha256": hashlib.sha256(payload.encode()).hexdigest(),
                       "timings_ms": times, "median_ms": medians,
                       "baseline_over_optimized": medians["baseline"] / medians["optimized"],
                       "python_over_optimized": medians["python"] / medians["optimized"],
                       "result_sha256": hashlib.sha256(json.dumps(expected, sort_keys=True).encode()).hexdigest()}
                rows.append(row)
                print(json.dumps({key: value for key, value in row.items() if key != "timings_ms"}), flush=True)
    finally:
        cleanup_errors = []
        for name, process in zip(names, processes):
            try:
                resources[name] = finish(process)
            except Exception as error:
                cleanup_errors.append(str(error))
        for log in logs:
            log.close()
        if cleanup_errors and sys.exc_info()[0] is None:
            raise RuntimeError("; ".join(cleanup_errors))
    validate(args.candidate_manifest, args.candidate)
    assert sha(args.baseline) == baseline["binary_sha256"]
    assert sha(args.baseline_manifest) == baseline_manifest_sha256
    assert harness_sources() == harness, "Benchmark tools changed during measurement"
    report = {
        "passed": True, "measured_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "system": platform.platform(), "python": platform.python_version(), "samples_per_workload": args.samples,
        "machine": platform.machine(), "cpu_count": os.cpu_count(),
        "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
        "upstream_commit": json.loads((ROOT / "migration/FILES.json").read_text())["upstream_commit"],
        "warmups_per_workload": 2, "order": "cycle through all six worker-order permutations",
        "extraction_workers": "one unless workload is explicitly labeled 4_workers; same setting for all implementations",
        "transport": "persistent JSONL, identical request bytes and corpus paths for all three implementations",
        "profile": "release", "benchmark_sha256": sha(Path(__file__)),
        "harness_files": harness,
        "resource_limitations": "Peak RSS covers each worker process; Python process-pool children are not aggregated. Native before/after workers include all their threads.",
        "baseline": {"binary_sha256": baseline["binary_sha256"], "source_set_sha256": baseline["source_set_sha256"],
                     "manifest": str(args.baseline_manifest.resolve()), "manifest_sha256": sha(args.baseline_manifest)},
        "optimized": {"binary_sha256": candidate["binary_sha256"], "source_set_sha256": candidate["source_set_sha256"],
                      "manifest": str(args.candidate_manifest.resolve()), "manifest_sha256": sha(args.candidate_manifest)},
        "results": rows, "worker_resources": resources,
    }
    path = directory / f"{args.record}.json"
    path.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"passed": True, "report": str(path), "worker_resources": resources}), flush=True)


if __name__ == "__main__":
    main()
