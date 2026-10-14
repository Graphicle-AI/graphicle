#!/usr/bin/env python3
"""Measure file-save -> fresh MCP result latency for ``graphicle-graph`` (development only).

For each target repository the harness:

1. removes any previous ``<repo>/<GRAPHICLE_OUT>`` and runs a cold
   ``graphicle-graph extract <repo> --code-only`` (timed);
2. measures the plain CLI re-index path: append a uniquely named function to
   one source file, time ``graphicle-graph update <repo>`` until it exits, then
   confirm the new node is in ``graph.json`` (``--update-runs`` samples), and
   revert; the same edit is also timed through the internal JSON-Lines
   ``{"operation":"watch","action":"rebuild","changed_paths":[...]}`` request so
   "full re-extract" and "incremental" costs can be told apart;
3. starts ``graphicle-graph watch <repo> --debounce D`` and
   ``graphicle-graph serve <repo>/<GRAPHICLE_OUT>/graph.json`` (stdio MCP),
   completes the MCP ``initialize`` handshake, samples idle CPU%/RSS of both
   processes over ``--idle`` seconds (``ps -o time=,rss=``; no psutil needed),
   then ``--n`` times appends a unique function and tight-polls the MCP
   ``get_node`` tool until its text contains ``Node: <name>()``; wall-clock from
   the file ``close()`` to the first correct response is the sample.  The
   moment ``watch`` prints ``Rebuilt:`` on stderr is recorded too so the
   detection+rebuild part can be separated from the serve reload part;
4. reverts the edit and waits for the graph to settle.

Outputs (in ``--out``): ``samples.csv`` (every raw sample), ``summary.json``,
``commands.txt`` (exact command lines executed) and the child processes'
stderr logs.  Everything is stdlib-only.  The shipped Rust library/binary does
not import or invoke this module.
"""
from __future__ import annotations

import argparse
import csv
import datetime as _dt
import json
import os
import platform
import queue
import shutil
import statistics
import subprocess
import sys
import threading
import time
import uuid
from pathlib import Path

HERE = Path(__file__).resolve().parent
DEFAULT_BINARY = HERE.parent / "target" / "release" / "graphicle-graph"
OUT_NAME = os.environ.get("GRAPHICLE_OUT", ".graphicle")


def now_utc() -> str:
    return _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def pct(values, q):
    if not values:
        return None
    xs = sorted(values)
    if len(xs) == 1:
        return xs[0]
    k = (len(xs) - 1) * q
    lo, hi = int(k), min(int(k) + 1, len(xs) - 1)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)


def stats(values):
    vals = [v for v in values if v is not None]
    if not vals:
        return {"n": 0}
    return {
        "n": len(vals),
        "min": min(vals),
        "p50": pct(vals, 0.5),
        "p95": pct(vals, 0.95),
        "max": max(vals),
        "mean": statistics.fmean(vals),
    }


class Commands:
    def __init__(self, path: Path):
        self.path = path
        self.path.write_text("")

    def log(self, argv, cwd=None, env_note=""):
        line = " ".join(str(a) for a in argv)
        if cwd:
            line = f"(cd {cwd} && {line})"
        if env_note:
            line = f"{env_note} {line}"
        with self.path.open("a") as fh:
            fh.write(line + "\n")


def parse_ps_time(text: str) -> float:
    """macOS/BSD ``ps -o time`` -> seconds ("[[dd-]hh:]mm:ss.cc")."""
    text = text.strip()
    days = 0
    if "-" in text:
        d, text = text.split("-", 1)
        days = int(d)
    parts = text.split(":")
    secs = 0.0
    for p in parts:
        secs = secs * 60 + float(p)
    return days * 86400 + secs


def ps_sample(pids):
    """Return {pid: (cpu_seconds, rss_kib)} for live pids."""
    out = {}
    if not pids:
        return out
    argv = ["ps", "-o", "pid=,time=,rss=", "-p", ",".join(str(p) for p in pids)]
    res = subprocess.run(argv, capture_output=True, text=True)
    for line in res.stdout.splitlines():
        cols = line.split()
        if len(cols) >= 3:
            out[int(cols[0])] = (parse_ps_time(cols[1]), int(cols[2]))
    return out


class LineReader(threading.Thread):
    """Drain a pipe into a queue of (monotonic_ts, line); optionally tee to a log file."""

    def __init__(self, pipe, log_path: Path | None = None):
        super().__init__(daemon=True)
        self.pipe = pipe
        self.q: "queue.Queue[tuple[float, str]]" = queue.Queue()
        self.log = log_path.open("a") if log_path else None
        self.start()

    def run(self):
        for raw in self.pipe:
            line = raw.decode("utf-8", "replace").rstrip("\n")
            ts = time.perf_counter()
            self.q.put((ts, line))
            if self.log:
                self.log.write(f"{ts:.6f} {line}\n")
                self.log.flush()
        self.q.put((time.perf_counter(), None))

    def drain(self):
        items = []
        while True:
            try:
                items.append(self.q.get_nowait())
            except queue.Empty:
                return items

    def wait_for(self, predicate, timeout: float):
        """Block until a line satisfies predicate; return (ts, line) or None."""
        deadline = time.perf_counter() + timeout
        while True:
            remaining = deadline - time.perf_counter()
            if remaining <= 0:
                return None
            try:
                ts, line = self.q.get(timeout=remaining)
            except queue.Empty:
                return None
            if line is None:
                return None
            if predicate(line):
                return ts, line


class JsonLinesClient:
    """Newline-delimited JSON over a child's stdin/stdout (MCP stdio or --jsonl)."""

    def __init__(self, argv, cwd, env, stderr_log: Path, cmds: Commands, env_note=""):
        cmds.log(argv, cwd=cwd, env_note=env_note)
        self.proc = subprocess.Popen(
            argv,
            cwd=cwd,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.stdout = LineReader(self.proc.stdout)
        self.stderr = LineReader(self.proc.stderr, stderr_log)
        self._id = 0

    def send(self, obj) -> None:
        self.proc.stdin.write((json.dumps(obj) + "\n").encode())
        self.proc.stdin.flush()

    def recv(self, timeout: float):
        try:
            ts, line = self.stdout.q.get(timeout=timeout)
        except queue.Empty:
            raise TimeoutError("no response from child")
        if line is None:
            raise RuntimeError("child closed stdout")
        return json.loads(line)

    def rpc(self, method, params=None, timeout=60.0):
        self._id += 1
        req = {"jsonrpc": "2.0", "id": self._id, "method": method}
        if params is not None:
            req["params"] = params
        self.send(req)
        while True:
            resp = self.recv(timeout)
            if resp.get("id") == self._id:
                return resp

    def notify(self, method, params=None):
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        self.send(msg)

    def tool_text(self, name, arguments, timeout=60.0) -> str:
        resp = self.rpc("tools/call", {"name": name, "arguments": arguments}, timeout)
        if "error" in resp:
            return "RPC-ERROR " + json.dumps(resp["error"])
        content = resp.get("result", {}).get("content", [])
        return "\n".join(c.get("text", "") for c in content if isinstance(c, dict))

    def close(self):
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()


def graph_counts(graph_json: Path):
    with graph_json.open("rb") as fh:
        g = json.load(fh)
    return len(g.get("nodes", [])), len(g.get("links", []))


def graph_has_label(graph_json: Path, label: str) -> bool:
    with graph_json.open("rb") as fh:
        g = json.load(fh)
    return any(n.get("label") == label for n in g.get("nodes", []))


def pick_edit_file(repo: Path, hint: str | None) -> Path:
    if hint:
        p = repo / hint
        if not p.is_file():
            sys.exit(f"--edit-file {hint} not found under {repo}")
        return p
    best = None
    for p in sorted(repo.rglob("*.py")):
        rel = p.relative_to(repo).as_posix()
        if any(part.startswith(".") for part in p.parts) or "/tests/" in f"/{rel}":
            continue
        if rel.startswith(("docs", "examples", "scripts")):
            continue
        size = p.stat().st_size
        if 2000 <= size <= 20000 and (best is None or size < best[0]):
            best = (size, p)
    if best is None:
        sys.exit(f"no suitable .py file found under {repo}; pass --edit-file")
    return best[1]


def probe_source(name: str, i: int) -> str:
    return f"\n\ndef {name}():\n    return {i}\n"


def run_timed(argv, cwd, env, cmds: Commands, log_path: Path, env_note=""):
    cmds.log(argv, cwd=cwd, env_note=env_note)
    t0 = time.perf_counter()
    res = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, text=True)
    dt = time.perf_counter() - t0
    with log_path.open("a") as fh:
        fh.write(f"$ {' '.join(map(str, argv))}\n{res.stderr}{res.stdout}\n")
    return dt, res


def live_trials(binary, repo, env, out_dir, name, cmds, env_note, graph_json,
                edit_file, restore, writer, summary, args):
    """`serve --watch`: one process, file events applied to the resident graph.

    The sample is file close() -> first `get_node` response containing the new
    node. Between trials the harness waits for the background incremental
    rebuild of the output folder ("Live rebuild") so trials do not overlap it.
    """
    serve = JsonLinesClient(
        [binary, "serve", str(graph_json), "--watch", "--root", str(repo)], repo, env,
        out_dir / f"{name}-serve-live.stderr.log", cmds, env_note,
    )
    try:
        init = serve.rpc("initialize", {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "bench_watch_latency", "version": "1"},
        })
        summary["mcp_server_info"] = init.get("result", {}).get("serverInfo")
        serve.notify("notifications/initialized")
        if serve.stderr.wait_for(lambda l: l.startswith("Live: watching"), 30) is None:
            raise RuntimeError("serve --watch did not start watching")
        summary["mcp_graph_stats_initial"] = serve.tool_text("graph_stats", {})
        pid = serve.proc.pid
        s0 = ps_sample([pid])
        t0 = time.perf_counter()
        rss = []
        while time.perf_counter() - t0 < args.idle:
            time.sleep(1.0)
            sample = ps_sample([pid])
            if pid in sample:
                rss.append(sample[pid][1])
        s1 = ps_sample([pid])
        elapsed = time.perf_counter() - t0
        summary["idle"] = {
            "serve_live": {
                "pid": pid,
                "cpu_percent": (s1[pid][0] - s0[pid][0]) / elapsed * 100 if pid in s0 and pid in s1 else None,
                "rss_mib_mean": statistics.fmean(rss) / 1024 if rss else None,
                "rss_mib_max": max(rss) / 1024 if rss else None,
            },
            "window_s": elapsed,
        }
        print(f"   idle: serve --watch cpu={summary['idle']['serve_live']['cpu_percent']:.2f}%"
              f" rss={summary['idle']['serve_live']['rss_mib_max']:.1f}MiB", flush=True)
        lat, patch_lat, rebuild_lat, polls_all = [], [], [], []
        for i in range(args.n):
            pname = f"bench_probe_live_{i}_{uuid.uuid4().hex[:6]}"
            label = f"{pname}()"
            serve.stderr.drain()
            with edit_file.open("ab") as fh:
                fh.write(probe_source(pname, i).encode())
            t_write = time.perf_counter()
            t_hit, polls = None, 0
            deadline = t_write + args.timeout
            while time.perf_counter() < deadline:
                text = serve.tool_text("get_node", {"label": pname}, timeout=args.timeout)
                polls += 1
                if f"Node: {label}" in text:
                    t_hit = time.perf_counter()
                    break
                time.sleep(args.poll_interval)
            t_patch = next((ts for ts, line in serve.stderr.drain()
                            if line and line.startswith("Live update") and ts >= t_write), None)
            l = (t_hit - t_write) if t_hit else None
            lat.append(l)
            patch_lat.append((t_patch - t_write) if t_patch else None)
            polls_all.append(polls)
            # The addition's background rebuild, then the revert and its rebuild.
            got = serve.stderr.wait_for(lambda ln: ln.startswith("Live rebuild"), args.timeout)
            rebuild_lat.append((got[0] - t_write) if got else None)
            writer.writerow([name, "live_to_mcp", i, pname,
                             f"{l:.6f}" if l is not None else "",
                             f"{got[0] - t_write:.6f}" if got else "", polls, int(t_hit is not None), ""])
            print(f"   trial {i:2d}: {'%.3f' % l if l else 'TIMEOUT'} s"
                  f" (in-memory update at {'%.3f' % patch_lat[-1] if patch_lat[-1] else '?'} s,"
                  f" {OUT_NAME} rebuilt at {'%.3f' % rebuild_lat[-1] if rebuild_lat[-1] else '?'} s, {polls} polls)",
                  flush=True)
            restore()
            if serve.stderr.wait_for(lambda ln: ln.startswith("Live rebuild"), args.timeout) is None:
                print("   WARNING: no Live rebuild after revert", flush=True)
            time.sleep(args.settle)
        summary["watch_to_mcp_s"] = stats(lat)
        summary["watch_to_mcp_timeouts"] = sum(1 for v in lat if v is None)
        summary["live_write_to_update_s"] = stats(patch_lat)
        summary["live_write_to_rebuilt_s"] = stats(rebuild_lat)
        summary["watch_polls_per_trial"] = stats(polls_all)
        summary["mcp_graph_stats_final"] = serve.tool_text("graph_stats", {})
        summary["loadavg_end"] = list(os.getloadavg())
    finally:
        serve.close()


def bench_repo(repo: Path, args, out_dir: Path, cmds: Commands, writer):
    name = repo.name
    binary = str(Path(args.binary).resolve())
    env = dict(os.environ)
    env["GRAPHICLE_OUT"] = OUT_NAME
    env_note = f"GRAPHICLE_OUT={OUT_NAME}"
    env.pop("OPENAI_API_KEY", None)
    env.pop("ANTHROPIC_API_KEY", None)
    env.pop("GEMINI_API_KEY", None)
    graph_json = repo / OUT_NAME / "graph.json"
    log_path = out_dir / f"{name}-cli.log"
    log_path.write_text("")
    edit_file = pick_edit_file(repo, args.edit_file)
    original = edit_file.read_bytes()
    summary = {
        "repo": name,
        "path": str(repo),
        "edit_file": edit_file.relative_to(repo).as_posix(),
        "graph_dir": OUT_NAME,
        "loadavg_start": list(os.getloadavg()),
    }
    print(f"== {name} ({repo}) edit_file={summary['edit_file']} loadavg={summary['loadavg_start']}", flush=True)

    def restore():
        edit_file.write_bytes(original)

    try:
        # (a) cold extract
        out_path = repo / OUT_NAME
        if out_path.exists():
            shutil.rmtree(out_path)
        argv = [binary, "extract", str(repo), "--code-only", "--timing"]
        dt, res = run_timed(argv, repo, env, cmds, log_path, env_note)
        if res.returncode != 0 or not graph_json.exists():
            raise RuntimeError(f"cold extract failed rc={res.returncode}\n{res.stderr[-2000:]}")
        code_files = None
        for line in (res.stdout + "\n" + res.stderr).splitlines():
            if "code and" in line and "files found" in line:
                try:
                    code_files = int(line.split("]", 1)[1].split()[0])
                except (ValueError, IndexError):
                    pass
        nodes, edges = graph_counts(graph_json)
        summary["cold_extract_s"] = dt
        summary["code_files"] = code_files
        summary["nodes"] = nodes
        summary["edges"] = edges
        print(f"   cold extract {dt:.2f}s files={code_files} nodes={nodes} edges={edges}", flush=True)

        # (4) plain `update` path, full CLI rebuild
        upd = []
        for i in range(args.update_runs):
            pname = f"bench_probe_upd_{i}_{uuid.uuid4().hex[:6]}"
            with edit_file.open("ab") as fh:
                fh.write(probe_source(pname, i).encode())
            argv = [binary, "update", str(repo)]
            dt, res = run_timed(argv, repo, env, cmds, log_path, env_note)
            ok = res.returncode == 0 and graph_has_label(graph_json, f"{pname}()")
            upd.append(dt if ok else None)
            writer.writerow([name, "update_cli", i, pname, f"{dt:.6f}", "", "", int(ok), ""])
            restore()
            run_timed(argv, repo, env, cmds, log_path, env_note)  # settle (untimed)
        summary["update_cli_s"] = stats(upd)
        summary["update_cli_failures"] = sum(1 for v in upd if v is None)
        print(f"   update(cli) p50={summary['update_cli_s'].get('p50')}", flush=True)

        # (4b) incremental rebuild through the internal JSON-Lines API
        inc = []
        client = JsonLinesClient(
            [binary, "--jsonl"], repo, env, out_dir / f"{name}-jsonl.stderr.log", cmds, env_note
        )
        try:
            for i in range(args.update_runs):
                pname = f"bench_probe_inc_{i}_{uuid.uuid4().hex[:6]}"
                with edit_file.open("ab") as fh:
                    fh.write(probe_source(pname, i).encode())
                req = {
                    "operation": "watch",
                    "action": "rebuild",
                    "path": str(repo),
                    "changed_paths": [str(edit_file)],
                }
                t0 = time.perf_counter()
                client.send(req)
                resp = client.recv(600)
                dt = time.perf_counter() - t0
                ok = bool(resp.get("ok", True)) and graph_has_label(graph_json, f"{pname}()")
                if i == 0:
                    summary["update_incremental_first_response"] = json.dumps(resp)[:400]
                inc.append(dt if ok else None)
                writer.writerow([name, "update_incremental_jsonl", i, pname, f"{dt:.6f}", "", "", int(ok), ""])
                restore()
                client.send(req)
                client.recv(600)  # settle (untimed)
        finally:
            client.close()
        summary["update_incremental_jsonl_s"] = stats(inc)
        summary["update_incremental_failures"] = sum(1 for v in inc if v is None)
        print(f"   update(incremental jsonl) p50={summary['update_incremental_jsonl_s'].get('p50')}", flush=True)

        # (b)+(c) watch + serve
        if args.live:
            live_trials(binary, repo, env, out_dir, name, cmds, env_note, graph_json,
                        edit_file, restore, writer, summary, args)
            summary["sub_second_p50"] = (summary.get("watch_to_mcp_s", {}).get("p50") or 9e9) < 1.0
            summary["sub_second_p95"] = (summary.get("watch_to_mcp_s", {}).get("p95") or 9e9) < 1.0
            return summary
        watch_argv = [binary, "watch", str(repo), "--debounce", str(args.debounce)] + list(args.watch_arg or [])
        cmds.log(watch_argv, cwd=repo, env_note=env_note)
        watch = subprocess.Popen(
            watch_argv, cwd=repo, env=env, stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        )
        watch_err = LineReader(watch.stderr, out_dir / f"{name}-watch.stderr.log")
        if watch_err.wait_for(lambda l: l.startswith("Watching"), 30) is None:
            raise RuntimeError("watch did not announce 'Watching'")
        serve = JsonLinesClient(
            [binary, "serve", str(graph_json)], repo, env,
            out_dir / f"{name}-serve.stderr.log", cmds, env_note,
        )
        try:
            init = serve.rpc("initialize", {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "bench_watch_latency", "version": "1"},
            })
            summary["mcp_server_info"] = init.get("result", {}).get("serverInfo")
            serve.notify("notifications/initialized")
            summary["mcp_graph_stats_initial"] = serve.tool_text("graph_stats", {})
            time.sleep(1.0)
            watch_err.drain()

            # idle CPU / RSS window
            pids = [watch.pid, serve.proc.pid]
            s0 = ps_sample(pids)
            t0 = time.perf_counter()
            rss = {p: [] for p in pids}
            while time.perf_counter() - t0 < args.idle:
                time.sleep(1.0)
                for p, (_, r) in ps_sample(pids).items():
                    rss[p].append(r)
            s1 = ps_sample(pids)
            elapsed = time.perf_counter() - t0
            idle = {}
            for label, pid in (("watch", watch.pid), ("serve", serve.proc.pid)):
                cpu = (s1[pid][0] - s0[pid][0]) / elapsed * 100 if pid in s0 and pid in s1 else None
                idle[label] = {
                    "pid": pid,
                    "cpu_percent": cpu,
                    "rss_mib_mean": statistics.fmean(rss[pid]) / 1024 if rss[pid] else None,
                    "rss_mib_max": max(rss[pid]) / 1024 if rss[pid] else None,
                }
            idle["window_s"] = elapsed
            summary["idle"] = idle
            print(f"   idle: watch cpu={idle['watch']['cpu_percent']:.2f}% rss={idle['watch']['rss_mib_max']:.1f}MiB;"
                  f" serve cpu={idle['serve']['cpu_percent']:.2f}% rss={idle['serve']['rss_mib_max']:.1f}MiB", flush=True)
            unexpected = [l for _, l in watch_err.drain() if l and "Rebuilt" in l]
            summary["idle_unexpected_rebuilds"] = len(unexpected)

            # (d) N edit -> MCP freshness trials
            lat, rebuilt_lat, reload_lat, polls_all = [], [], [], []
            for i in range(args.n):
                pname = f"bench_probe_watch_{i}_{uuid.uuid4().hex[:6]}"
                label = f"{pname}()"
                watch_err.drain()
                with edit_file.open("ab") as fh:
                    fh.write(probe_source(pname, i).encode())
                t_write = time.perf_counter()
                t_hit = None
                polls = 0
                deadline = t_write + args.timeout
                while time.perf_counter() < deadline:
                    text = serve.tool_text("get_node", {"label": pname}, timeout=args.timeout)
                    polls += 1
                    if f"Node: {label}" in text:
                        t_hit = time.perf_counter()
                        break
                    time.sleep(args.poll_interval)
                t_rebuilt = None
                for ts, line in watch_err.drain():
                    if line and line.startswith("Rebuilt") and t_rebuilt is None and ts >= t_write:
                        t_rebuilt = ts
                if t_rebuilt is None:
                    # graph.json is written before watch prints "Rebuilt:", so the MCP
                    # hit can precede the line; consume it so the revert wait below does
                    # not mistake it for the revert's own rebuild.
                    got = watch_err.wait_for(lambda ln: ln.startswith("Rebuilt") or ln.startswith("Rebuild failed")
                                             or ln.startswith("Rebuild skipped"),
                                             args.timeout if t_hit is None else 30.0)
                    if got is not None and got[1].startswith("Rebuilt"):
                        t_rebuilt = got[0]
                l = (t_hit - t_write) if t_hit else None
                rl = (t_rebuilt - t_write) if t_rebuilt else None
                lat.append(l)
                rebuilt_lat.append(rl)
                reload_lat.append((t_hit - t_rebuilt) if (t_hit and t_rebuilt and t_hit >= t_rebuilt) else None)
                polls_all.append(polls)
                writer.writerow([
                    name, "watch_to_mcp", i, pname,
                    f"{l:.6f}" if l is not None else "",
                    f"{rl:.6f}" if rl is not None else "",
                    polls, int(t_hit is not None), "",
                ])
                print(f"   trial {i:2d}: {'%.3f' % l if l else 'TIMEOUT'} s (watch Rebuilt at {'%.3f' % rl if rl else '?'} s, {polls} polls)", flush=True)
                # (e) revert and wait for watch to settle
                restore()
                t_rev = time.perf_counter()
                settled = watch_err.wait_for(lambda ln: ln.startswith("Rebuilt") or ln.startswith("Rebuild failed")
                                             or ln.startswith("Rebuild skipped"), args.timeout)
                if settled is None:
                    print("   WARNING: no Rebuilt after revert", flush=True)
                time.sleep(max(0.0, args.settle - (time.perf_counter() - t_rev)))
            summary["watch_to_mcp_s"] = stats(lat)
            summary["watch_to_mcp_timeouts"] = sum(1 for v in lat if v is None)
            summary["watch_write_to_rebuilt_s"] = stats(rebuilt_lat)
            summary["watch_rebuilt_to_mcp_s"] = stats(reload_lat)
            summary["watch_polls_per_trial"] = stats(polls_all)
            summary["mcp_graph_stats_final"] = serve.tool_text("graph_stats", {})
            summary["loadavg_end"] = list(os.getloadavg())
        finally:
            serve.close()
            watch.terminate()
            try:
                watch.wait(timeout=10)
            except subprocess.TimeoutExpired:
                watch.kill()
                watch.wait()
    finally:
        restore()
        if args.keep_out is False:
            shutil.rmtree(repo / OUT_NAME, ignore_errors=True)
    summary["sub_second_p50"] = (summary.get("watch_to_mcp_s", {}).get("p50") or 9e9) < 1.0
    summary["sub_second_p95"] = (summary.get("watch_to_mcp_s", {}).get("p95") or 9e9) < 1.0
    return summary


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--binary", default=str(DEFAULT_BINARY))
    ap.add_argument("--repo", action="append", required=True, help="repository root (repeatable)")
    ap.add_argument("--edit-file", default=None, help="repo-relative .py file to edit (default: auto-pick)")
    ap.add_argument("--out", required=True, help="output directory")
    ap.add_argument("--n", type=int, default=20, help="watch->MCP trials per repo")
    ap.add_argument("--update-runs", type=int, default=10)
    ap.add_argument("--idle", type=float, default=30.0, help="idle sampling window (s)")
    ap.add_argument("--debounce", type=float, default=0.01,
                    help="passed to `watch --debounce` (must be > 0; CLI default is 2.0 s)")
    ap.add_argument("--poll-interval", type=float, default=0.02)
    ap.add_argument("--watch-arg", action="append", default=None,
                    help="extra argument appended to the `watch` command line (repeatable)")
    ap.add_argument("--timeout", type=float, default=180.0)
    ap.add_argument("--settle", type=float, default=2.0, help="min seconds between trials after revert")
    ap.add_argument("--keep-out", action="store_true", help="leave <repo>/<GRAPHICLE_OUT> in place")
    ap.add_argument("--live", action="store_true",
                    help="measure `serve --watch` (one resident process) instead of watch + serve")
    args = ap.parse_args()

    out_dir = Path(args.out).resolve()
    out_dir.mkdir(parents=True, exist_ok=True)
    cmds = Commands(out_dir / "commands.txt")
    cmds.log(sys.argv)
    binary = Path(args.binary).resolve()
    sha = subprocess.run(["shasum", "-a", "256", str(binary)], capture_output=True, text=True).stdout.split()[0]
    hw = subprocess.run(["sysctl", "-n", "machdep.cpu.brand_string", "hw.ncpu", "hw.memsize"], capture_output=True, text=True).stdout.split("\n")
    meta = {
        "started_utc": now_utc(),
        "binary": str(binary),
        "binary_sha256": sha,
        "python": sys.version.split()[0],
        "platform": platform.platform(),
        "hardware": {"cpu": hw[0] if hw else None, "ncpu": hw[1] if len(hw) > 1 else None, "memsize": hw[2] if len(hw) > 2 else None},
        "args": vars(args),
        "watch_debounce_s": args.debounce,
        "watch_poll_interval_s_from_source": 0.5,
        "watch_extra_args": list(args.watch_arg or []),
        "mechanism": {
            "watch": ("src/watch.rs watch(): sleep(500ms) + full-tree stat snapshot, debounce, then "
                      "rebuild(path, RebuildOptions::default()) => full re-extract (parse cache) "
                      "+ reconcile + cluster + atomic graph.json write"),
            "serve": "src/serve.rs GraphContextCache::load(): every tools/call re-stats graph.json (len,mtime) and reloads the graph on change; no manual step",
        },
    }
    repos = []
    with (out_dir / "samples.csv").open("w", newline="") as fh:
        writer = csv.writer(fh)
        writer.writerow(["repo", "kind", "trial", "probe", "latency_s", "watch_rebuilt_s", "polls", "ok", "note"])
        for r in args.repo:
            repo = Path(r).resolve()
            if not repo.is_dir():
                sys.exit(f"not a directory: {repo}")
            try:
                repos.append(bench_repo(repo, args, out_dir, cmds, writer))
            except Exception as e:  # keep going, record failure
                repos.append({"repo": repo.name, "path": str(repo), "error": repr(e)})
                print(f"   ERROR: {e!r}", flush=True)
            fh.flush()
    meta["finished_utc"] = now_utc()
    meta["repos"] = repos
    (out_dir / "summary.json").write_text(json.dumps(meta, indent=2, default=str) + "\n")
    print(f"wrote {out_dir}/summary.json", flush=True)


if __name__ == "__main__":
    main()
