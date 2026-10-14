#!/usr/bin/env python3
"""Benchmark harness: Python Graphify (the graphifyy release installed in .reference-venv) vs the native Rust port.

Stdlib only.  docs/benchmarks.md has the published speed comparison.

For every repo x implementation x repetition it measures, offline (no LLM):

  cold    rm -rf <out>; <impl> extract <repo> --code-only --force
  update  append one probe function to a chosen source file, then
          <impl> update <repo>            (file restored via git checkout)
  query   <impl> query "<question>" --graph <out>/graph.json --budget 2000

Each phase records wall time, user/sys CPU, peak RSS (largest single process,
from /usr/bin/time -l), peak RSS of the whole process tree (100 ms ps sampling),
exit code and, for cold/update, node/edge counts read from graph.json.
Implementation order alternates every repetition to spread thermal drift.
"""
from __future__ import annotations

import argparse
import csv
import datetime as dt
import hashlib
import json
import os
import platform
import re
import shutil
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent / "crates" / "graphicle-graph"
NATIVE_BIN = ROOT / "target" / "release" / "graphicle-graph"
PY_VENV_PYTHON = ROOT / ".reference-venv" / "bin" / "python"
TIME_BIN = "/usr/bin/time"

REPOS = {
    # name: (github slug, probe file, probe language)
    "click": ("pallets/click", "src/click/utils.py", "py"),
    "httpx": ("encode/httpx", "httpx/_utils.py", "py"),
    "excalidraw": ("excalidraw/excalidraw", "packages/excalidraw/deburr.ts", "ts"),
    # Medium and large Python corpora.
    "fastapi": ("fastapi/fastapi", "fastapi/utils.py", "py"),
    "django": ("django/django", "django/utils/text.py", "py"),
}
QUERIES = {
    "click": "how does command parsing work",
    "httpx": "how are requests sent through the transport",
    "excalidraw": "how is the canvas rendered",
    "fastapi": "how are path operations registered",
    "django": "how are url patterns resolved",
}
PROBES = {
    "py": '\n\ndef _bench_probe_function(value):\n    """Benchmark probe (appended by bench/run.py)."""\n    return str(value).strip()\n',
    "ts": "\n\nexport function benchProbeFunction(value: string): string {\n  // Benchmark probe (appended by bench/run.py)\n  return value.trim();\n}\n",
}
SECRET_ENV_RE = re.compile(r"(API_KEY|_TOKEN|SECRET|ANTHROPIC_|OPENAI_|GEMINI_|GOOGLE_|MOONSHOT_|DEEPSEEK_|OLLAMA_)", re.I)


def sh(cmd, **kw):
    return subprocess.run(cmd, text=True, capture_output=True, **kw)


def loadavg():
    r = sh(["sysctl", "-n", "vm.loadavg"])
    m = re.findall(r"[\d.]+", r.stdout)
    return [float(x) for x in m[:3]] if len(m) >= 3 else list(os.getloadavg())


def machine_info():
    def s(*a):
        r = sh(list(a))
        return r.stdout.strip() if r.returncode == 0 else None
    return {
        "cpu": s("sysctl", "-n", "machdep.cpu.brand_string"),
        "mem_bytes": int(s("sysctl", "-n", "hw.memsize") or 0),
        "ncpu": os.cpu_count(),
        "macos": s("sw_vers", "-productVersion"),
        "macos_build": s("sw_vers", "-buildVersion"),
        "platform": platform.platform(),
        "rustc": s("rustc", "--version"),
        "harness_python": sys.version.split()[0],
        "venv_python": s(str(PY_VENV_PYTHON), "--version"),
        "native_binary": str(NATIVE_BIN),
        "native_sha256": hashlib.sha256(NATIVE_BIN.read_bytes()).hexdigest(),
        "power_source": s("pmset", "-g", "batt"),
    }


def clean_env(out_dir: Path):
    env = {k: v for k, v in os.environ.items() if not SECRET_ENV_RE.search(k)}
    env["GRAPHIFY_OUT"] = str(out_dir)
    env["GRAPHIFY_NO_TIPS"] = "1"
    # Python Graphify 0.9.72 refreshes stale installed skills on any CLI run
    # (#1805); a benchmark must not rewrite the skills of the user running it.
    env["GRAPHIFY_NO_AUTO_REFRESH"] = "1"
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    return env


class TreeRssSampler(threading.Thread):
    """Sums RSS of every process in the child's process group every 100 ms."""

    def __init__(self, pgid: int):
        super().__init__(daemon=True)
        self.pgid, self.peak, self.samples, self._halt = pgid, 0, 0, threading.Event()

    def run(self):
        while not self._halt.is_set():
            r = sh(["ps", "-axo", "pgid=,rss="])
            total = 0
            for line in r.stdout.splitlines():
                parts = line.split()
                if len(parts) == 2 and parts[0] == str(self.pgid):
                    total += int(parts[1]) * 1024
            self.peak = max(self.peak, total)
            self.samples += 1
            self._halt.wait(0.1)

    def stop(self):
        self._halt.set()
        self.join()


def timed_run(cmd, env, cwd, timeout):
    """Run cmd under /usr/bin/time -l; return metrics dict."""
    full = [TIME_BIN, "-l"] + cmd
    t0 = time.perf_counter()
    proc = subprocess.Popen(full, env=env, cwd=cwd, text=True, start_new_session=True,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    sampler = TreeRssSampler(proc.pid)
    sampler.start()
    try:
        out, err = proc.communicate(timeout=timeout)
        timed_out = False
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, 9)
        out, err = proc.communicate()
        timed_out = True
    wall = time.perf_counter() - t0
    sampler.stop()
    m = {"wall_s": round(wall, 4), "exit_code": proc.returncode, "timed_out": timed_out,
         "user_s": None, "sys_s": None, "peak_rss_max_proc_bytes": None,
         "peak_rss_tree_bytes": sampler.peak, "rss_samples": sampler.samples,
         "stdout": out, "stderr": err}
    mm = re.search(r"([\d.]+) real\s+([\d.]+) user\s+([\d.]+) sys", err)
    if mm:
        m["user_s"], m["sys_s"] = float(mm.group(2)), float(mm.group(3))
    mr = re.search(r"(\d+)\s+maximum resident set size", err)
    if mr:
        m["peak_rss_max_proc_bytes"] = int(mr.group(1))
    return m


def graph_counts(path: Path):
    if not path.exists():
        return None, None, None, None
    g = json.loads(path.read_text())
    nodes = g.get("nodes", [])
    edges = g.get("links", g.get("edges", []))
    comms = {n.get("community") for n in nodes if n.get("community") is not None}
    h = hashlib.sha256()
    for n in sorted(nodes, key=lambda n: str(n.get("id"))):
        h.update(str(n.get("id")).encode())
    return len(nodes), len(edges), len(comms), h.hexdigest()[:16]


def impl_cmd(impl: str):
    return [str(NATIVE_BIN)] if impl == "native" else [str(PY_VENV_PYTHON), "-m", "graphify"]


def install_sizes():
    native = NATIVE_BIN.stat().st_size
    script = r'''
import importlib.metadata as md, re, os, json
seen = {}
def reqs(d):
    out = []
    for r in d.requires or []:
        if "extra ==" in r: continue
        out.append(re.split(r"[\s;<>=!\[(~]", r, 1)[0].lower())
    return out
def walk(name):
    key = name.lower().replace("_", "-")
    if key in seen: return
    try: d = md.distribution(name)
    except md.PackageNotFoundError:
        seen[key] = None; return
    total = 0
    for f in d.files or []:
        try: total += os.path.getsize(f.locate())
        except OSError: pass
    seen[key] = {"version": d.version, "bytes": total, "location": str(d.locate_file(""))}
    for r in reqs(d): walk(r)
walk("graphifyy")
print(json.dumps(seen))
'''
    r = sh([str(PY_VENV_PYTHON), "-c", script])
    dists = json.loads(r.stdout)
    total = sum(v["bytes"] for v in dists.values() if v)
    return {
        "native_binary_bytes": native,
        "python_closure_bytes": total,
        "python_closure_method": "sum of RECORD file sizes of dist 'graphifyy' plus its transitive "
                                 "Requires-Dist closure excluding 'extra ==' requirements, resolved "
                                 "via importlib.metadata inside .reference-venv (CPython itself excluded)",
        "python_dists": dists,
        "python_dists_not_installed": [k for k, v in dists.items() if v is None],
    }


def git(repo: Path, *args):
    return sh(["git", "-C", str(repo)] + list(args)).stdout.strip()


def ensure_repo(name: str, repos_dir: Path, allow_clone: bool):
    slug = REPOS[name][0]
    path = repos_dir / name
    if not path.exists():
        if not allow_clone:
            raise SystemExit(f"{path} missing and --no-clone given")
        repos_dir.mkdir(parents=True, exist_ok=True)
        r = sh(["git", "clone", "--depth", "1", f"https://github.com/{slug}.git", str(path)])
        if r.returncode != 0:
            raise SystemExit(f"clone {slug} failed: {r.stderr}")
    return path


def median(xs):
    xs = [x for x in xs if x is not None]
    return round(statistics.median(xs), 4) if xs else None


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--reps", type=int, default=5)
    ap.add_argument("--large-reps", type=int, default=3, help="reps for a repo whose first Python cold run exceeds --large-threshold")
    ap.add_argument("--large-threshold", type=float, default=1200.0, help="seconds")
    ap.add_argument("--repos", default=",".join(REPOS))
    ap.add_argument("--repos-dir", type=Path, default=Path("/tmp/gf-bench-repos"))
    ap.add_argument("--no-clone", action="store_true")
    ap.add_argument("--results-dir", type=Path, default=HERE / "results")
    ap.add_argument("--work-dir", type=Path, default=HERE / ".work", help="GRAPHIFY_OUT parent (deleted per cold run)")
    ap.add_argument("--timeout", type=float, default=3600.0, help="per-command timeout seconds")
    ap.add_argument("--impls", default="native,python")
    args = ap.parse_args()

    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    res_dir = args.results_dir / stamp
    res_dir.mkdir(parents=True, exist_ok=True)
    impls = args.impls.split(",")
    raw_rows = []
    raw_fields = ["utc", "repo", "sha", "impl", "rep", "phase", "wall_s", "user_s", "sys_s",
                  "peak_rss_max_proc_bytes", "peak_rss_tree_bytes", "rss_samples", "exit_code", "timed_out",
                  "nodes", "edges", "communities", "node_id_digest", "output_digest", "loadavg_1m", "loadavg_5m", "loadavg_15m",
                  "command"]
    raw_path = res_dir / "raw.csv"
    raw_fh = open(raw_path, "w", newline="")
    raw_w = csv.DictWriter(raw_fh, fieldnames=raw_fields)
    raw_w.writeheader()

    def record(row):
        raw_rows.append(row)
        raw_w.writerow({k: row.get(k) for k in raw_fields})
        raw_fh.flush()

    info = machine_info()
    sizes = install_sizes()
    summary = {"utc": stamp, "machine": info, "install_size": sizes, "reps_requested": args.reps,
               "repos_dir": str(args.repos_dir), "commands": {}, "repos": {}, "notes": []}
    log = open(res_dir / "log.txt", "w")

    def note(msg):
        print(msg, flush=True)
        log.write(msg + "\n")
        log.flush()

    note(f"[bench] results -> {res_dir}")
    note(f"[bench] machine: {info['cpu']} {info['mem_bytes'] // 2**30} GiB macOS {info['macos']} loadavg={loadavg()}")

    for name in args.repos.split(","):
        repo = ensure_repo(name, args.repos_dir, not args.no_clone)
        sha = git(repo, "rev-parse", "HEAD")
        nfiles = len(git(repo, "ls-files").splitlines())
        probe_rel, lang = REPOS[name][1], REPOS[name][2]
        probe = repo / probe_rel
        if git(repo, "status", "--porcelain"):
            note(f"[bench] WARNING {name}: working tree dirty before run")
        reps = args.reps
        rinfo = {"slug": REPOS[name][0], "path": str(repo), "sha": sha, "git_files": nfiles,
                 "probe_file": probe_rel, "query": QUERIES[name], "reps": reps, "impl": {}}
        summary["repos"][name] = rinfo
        note(f"[bench] === {name} {sha[:12]} files={nfiles}")
        rep = 0
        while rep < reps:
            order = impls if rep % 2 == 0 else list(reversed(impls))
            for impl in order:
                out_dir = args.work_dir / f"{impl}-{name}"
                env = clean_env(out_dir)
                base = impl_cmd(impl)
                la = loadavg()
                common = {"repo": name, "sha": sha, "impl": impl, "rep": rep,
                          "loadavg_1m": la[0], "loadavg_5m": la[1], "loadavg_15m": la[2]}

                # ---- cold extract
                shutil.rmtree(out_dir, ignore_errors=True)
                out_dir.mkdir(parents=True)
                cmd = base + ["extract", str(repo), "--code-only", "--force"]
                m = timed_run(cmd, env, str(ROOT), args.timeout)
                n, e, c, dg = graph_counts(out_dir / "graph.json")
                record({**common, "utc": dt.datetime.now(dt.timezone.utc).isoformat(), "phase": "cold",
                        **{k: m[k] for k in ("wall_s", "user_s", "sys_s", "peak_rss_max_proc_bytes", "peak_rss_tree_bytes", "rss_samples", "exit_code", "timed_out")},
                        "nodes": n, "edges": e, "communities": c, "node_id_digest": dg, "command": " ".join(cmd)})
                note(f"[bench] {name} rep{rep} {impl:6} cold   {m['wall_s']:8.2f}s rc={m['exit_code']} nodes={n} edges={e} comms={c} rss={m['peak_rss_max_proc_bytes']} tree={m['peak_rss_tree_bytes']} la={la[0]}")
                if m["exit_code"] != 0:
                    note(f"[bench]   stderr tail: {m['stderr'][-600:]}")
                summary["commands"]["cold"] = "<impl> extract <repo> --code-only --force   (env GRAPHIFY_OUT=<out>, out dir removed first)"
                if impl == "python" and rep == 0 and m["wall_s"] > args.large_threshold and reps > args.large_reps:
                    reps = rinfo["reps"] = args.large_reps
                    summary["notes"].append(f"{name}: Python cold run took {m['wall_s']:.0f}s > {args.large_threshold:.0f}s; repetitions reduced to {reps}")
                    note(summary["notes"][-1])

                # ---- single-file update
                if m["exit_code"] == 0 and probe.exists():
                    original = probe.read_bytes()
                    with open(probe, "ab") as fh:
                        fh.write(PROBES[lang].encode())
                    try:
                        cmd = base + ["update", str(repo)]
                        mu = timed_run(cmd, env, str(ROOT), args.timeout)
                    finally:
                        probe.write_bytes(original)
                        sh(["git", "-C", str(repo), "checkout", "--", probe_rel])
                    n, e, c, dg = graph_counts(out_dir / "graph.json")
                    record({**common, "utc": dt.datetime.now(dt.timezone.utc).isoformat(), "phase": "update",
                            **{k: mu[k] for k in ("wall_s", "user_s", "sys_s", "peak_rss_max_proc_bytes", "peak_rss_tree_bytes", "rss_samples", "exit_code", "timed_out")},
                            "nodes": n, "edges": e, "communities": c, "node_id_digest": dg, "command": " ".join(cmd)})
                    note(f"[bench] {name} rep{rep} {impl:6} update {mu['wall_s']:8.2f}s rc={mu['exit_code']} nodes={n} edges={e} comms={c} rss={mu['peak_rss_max_proc_bytes']} tree={mu['peak_rss_tree_bytes']}")
                    if mu["exit_code"] != 0:
                        note(f"[bench]   stderr tail: {mu['stderr'][-600:]}")
                    summary["commands"]["update"] = f"append probe function to <probe_file>; <impl> update <repo>; git checkout -- <probe_file>"

                # ---- query
                gpath = out_dir / "graph.json"
                if gpath.exists():
                    cmd = base + ["query", QUERIES[name], "--graph", str(gpath), "--budget", "2000"]
                    mq = timed_run(cmd, env, str(ROOT), args.timeout)
                    body = "\n".join(l for l in mq["stdout"].splitlines()[1:] if "warning" not in l.lower())
                    record({**common, "utc": dt.datetime.now(dt.timezone.utc).isoformat(), "phase": "query",
                            **{k: mq[k] for k in ("wall_s", "user_s", "sys_s", "peak_rss_max_proc_bytes", "peak_rss_tree_bytes", "rss_samples", "exit_code", "timed_out")},
                            "output_digest": hashlib.sha256(body.encode()).hexdigest()[:16], "nodes": len(body.splitlines()),
                            "command": " ".join(cmd)})
                    first = mq["stdout"].splitlines()[0] if mq["stdout"] else ""
                    note(f"[bench] {name} rep{rep} {impl:6} query  {mq['wall_s']:8.2f}s rc={mq['exit_code']} lines={len(body.splitlines())} | {first[:110]}")
                    summary["commands"]["query"] = '<impl> query "<question>" --graph <out>/graph.json --budget 2000'
            rep += 1

        # per-repo summary
        for impl in impls:
            d = {}
            for phase in ("cold", "update", "query"):
                rows = [r for r in raw_rows if r["repo"] == name and r["impl"] == impl and r["phase"] == phase]
                if not rows:
                    continue
                ok = [r for r in rows if r["exit_code"] == 0]
                d[phase] = {
                    "samples_wall_s": [r["wall_s"] for r in rows],
                    "median_wall_s": median([r["wall_s"] for r in ok]),
                    "median_user_s": median([r["user_s"] for r in ok]),
                    "median_sys_s": median([r["sys_s"] for r in ok]),
                    "median_peak_rss_max_proc_bytes": median([r["peak_rss_max_proc_bytes"] for r in ok]),
                    "max_peak_rss_max_proc_bytes": max([r["peak_rss_max_proc_bytes"] or 0 for r in rows]),
                    "median_peak_rss_tree_bytes": median([r["peak_rss_tree_bytes"] for r in ok]),
                    "max_peak_rss_tree_bytes": max([r["peak_rss_tree_bytes"] or 0 for r in rows]),
                    "exit_codes": [r["exit_code"] for r in rows],
                    "loadavg_1m_before": [r["loadavg_1m"] for r in rows],
                }
                if phase in ("cold", "update"):
                    d[phase]["nodes"] = sorted({r["nodes"] for r in rows})
                    d[phase]["edges"] = sorted({r["edges"] for r in rows})
                    d[phase]["communities"] = sorted({r["communities"] for r in rows})
                    d[phase]["node_id_digest"] = sorted({r["node_id_digest"] for r in rows})
                else:
                    d[phase]["output_digest"] = sorted({r["output_digest"] for r in rows})
                    d[phase]["output_lines"] = sorted({r["nodes"] for r in rows})
            rinfo["impl"][impl] = d
        if "native" in impls and "python" in impls:
            ratios = {}
            for phase in ("cold", "update", "query"):
                a = rinfo["impl"]["python"].get(phase, {}).get("median_wall_s")
                b = rinfo["impl"]["native"].get(phase, {}).get("median_wall_s")
                ratios[f"{phase}_python_over_native"] = round(a / b, 3) if a and b else None
            ra = rinfo["impl"]["python"].get("cold", {}).get("median_peak_rss_max_proc_bytes")
            rb = rinfo["impl"]["native"].get("cold", {}).get("median_peak_rss_max_proc_bytes")
            ratios["cold_rss_python_over_native"] = round(ra / rb, 3) if ra and rb else None
            rinfo["ratios"] = ratios
            for phase in ("cold", "update"):
                pn = rinfo["impl"]["python"].get(phase, {})
                nn = rinfo["impl"]["native"].get(phase, {})
                if pn and nn and (pn.get("nodes") != nn.get("nodes") or pn.get("edges") != nn.get("edges")):
                    summary["notes"].append(f"{name} {phase}: output differs - python nodes/edges {pn.get('nodes')}/{pn.get('edges')} vs native {nn.get('nodes')}/{nn.get('edges')}")
            pq = rinfo["impl"]["python"].get("query", {}).get("output_digest")
            nq = rinfo["impl"]["native"].get("query", {}).get("output_digest")
            if pq and nq and pq != nq:
                summary["notes"].append(f"{name} query: output text differs between implementations (digests {pq} vs {nq}); see README for what was compared")
        (res_dir / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")

    summary["install_size"]["python_over_native"] = round(sizes["python_closure_bytes"] / sizes["native_binary_bytes"], 3)
    summary["loadavg_end"] = loadavg()
    (res_dir / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")

    # summary.csv: one row per repo
    cols = ["repo", "slug", "sha", "git_files", "reps",
            "nodes_native", "edges_native", "nodes_python", "edges_python",
            "cold_median_s_native", "cold_median_s_python", "cold_speedup",
            "update_median_s_native", "update_median_s_python", "update_speedup",
            "query_median_s_native", "query_median_s_python", "query_speedup",
            "cold_peak_rss_native_bytes", "cold_peak_rss_python_bytes", "cold_peak_rss_tree_native_bytes", "cold_peak_rss_tree_python_bytes",
            "install_native_bytes", "install_python_bytes"]
    with open(res_dir / "summary.csv", "w", newline="") as fh:
        w = csv.writer(fh)
        w.writerow(cols)
        for name, r in summary["repos"].items():
            g = lambda impl, ph, k: r["impl"].get(impl, {}).get(ph, {}).get(k)
            fmt = lambda xs: "/".join(str(x) for x in xs) if xs else None
            w.writerow([name, r["slug"], r["sha"], r["git_files"], r["reps"],
                        fmt(g("native", "cold", "nodes")), fmt(g("native", "cold", "edges")),
                        fmt(g("python", "cold", "nodes")), fmt(g("python", "cold", "edges")),
                        g("native", "cold", "median_wall_s"), g("python", "cold", "median_wall_s"), r.get("ratios", {}).get("cold_python_over_native"),
                        g("native", "update", "median_wall_s"), g("python", "update", "median_wall_s"), r.get("ratios", {}).get("update_python_over_native"),
                        g("native", "query", "median_wall_s"), g("python", "query", "median_wall_s"), r.get("ratios", {}).get("query_python_over_native"),
                        g("native", "cold", "median_peak_rss_max_proc_bytes"), g("python", "cold", "median_peak_rss_max_proc_bytes"),
                        g("native", "cold", "median_peak_rss_tree_bytes"), g("python", "cold", "median_peak_rss_tree_bytes"),
                        sizes["native_binary_bytes"], sizes["python_closure_bytes"]])
    note(f"[bench] done -> {res_dir}")
    raw_fh.close()
    log.close()


if __name__ == "__main__":
    main()
