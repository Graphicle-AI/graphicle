#!/usr/bin/env python3
"""Store under load: index a large repository into a store that is serving reads.

Stdlib only. README.md beside this file says what it measures and how to run
it; docs/configuration.md lists the store settings it exercises.

One run:

  1. A fresh data folder. The seed repository is synced into it (`graphicle
     sync SEED --json`), so readers have a large code graph to query.
  2. `graphicle serve` on loopback, under `strace -f -c` for its fsyncs.
  3. N clients (default 33) call the server's HTTP MCP endpoint for the seed
     project in a loop, with a fixed mix of the graph tools (`MIX`), each
     client with its own MCP session, a new one every `--calls-per-pass`
     calls (a "pass").
  4. After `--warmup` seconds, the gate's indexing flow: `register` an empty
     project over the API, then sync the indexed repository into it with
     `graphicle sync PATH --project ID --json` as a separate process, under
     `strace -f -c`. Readers keep reading the seed project throughout.
  5. Readers go on for `--after` seconds once indexing ends, then stop.

Every `--sample` seconds (default 10) it records the store and WAL size, the
RSS of the server and of the indexer, and the reads completed. At the end it
writes `<out>/run.json` (the summary), `<out>/samples.csv`, `<out>/calls.csv`
and the processes' logs.
"""
from __future__ import annotations

import argparse
import csv
import json
import os
import random
import re
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

# The weighted mix of MCP tool calls, weighted to the graph queries. Names
# are the server's (`tools/list`); a tool the server does not list is
# dropped from the mix and reported. Each entry: (tool, weight, argument
# builder name).
MIX = [
    ("query_graph", 20, "question"),
    ("get_node", 14, "symbol"),
    ("get_neighbors", 14, "symbol"),
    ("shortest_path", 6, "pair"),
    ("graph_as_of", 6, "query"),
    ("god_nodes", 3, "none"),
    ("graph_stats", 3, "none"),
    ("recall_project", 10, "question"),
    ("read_project_source", 8, "file"),
    ("repository_files", 6, "prefix"),
    ("search_project_memory", 2, "question"),
    ("repository_status", 2, "none"),
]


def now() -> float:
    return time.monotonic()


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def rss_kib(pid: int | None) -> tuple[int, int]:
    """(VmRSS, VmHWM) of `pid` and its threads, in KiB; (0, 0) once gone."""
    if not pid:
        return 0, 0
    try:
        text = Path(f"/proc/{pid}/status").read_text()
    except OSError:
        return 0, 0
    def field(name: str) -> int:
        m = re.search(rf"^{name}:\s+(\d+) kB", text, re.M)
        return int(m.group(1)) if m else 0
    return field("VmRSS"), field("VmHWM")


def tree_pids(root: int) -> list[int]:
    """`root` and its descendants (strace wraps the real process)."""
    children: dict[int, list[int]] = {}
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            ppid = int(re.search(r"^PPid:\s+(\d+)", (entry / "status").read_text(), re.M).group(1))
        except (OSError, AttributeError):
            continue
        children.setdefault(ppid, []).append(int(entry.name))
    out, todo = [], [root]
    while todo:
        pid = todo.pop()
        out.append(pid)
        todo.extend(children.get(pid, []))
    return out


def graphicle_pid(root: int | None) -> int | None:
    """The graphicle process under `root` (itself, or strace's child)."""
    if not root:
        return None
    for pid in tree_pids(root):
        try:
            exe = os.readlink(f"/proc/{pid}/exe")
        except OSError:
            continue
        if os.path.basename(exe).startswith("graphicle"):
            return pid
    return None


def postgres_rss_kib() -> int:
    """Total RSS of the PostgreSQL server's processes (shared buffers are
    counted once per process that touched them, so this overstates)."""
    total = 0
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            if os.path.basename(os.readlink(entry / "exe")) != "postgres":
                continue
        except OSError:
            continue
        total += rss_kib(int(entry.name))[0]
    return total


def postgres_pss_kib() -> int:
    """Proportional set size of the PostgreSQL server's processes: shared
    memory divided among the processes that map it, so the sum is real."""
    total = 0
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            if os.path.basename(os.readlink(entry / "exe")) != "postgres":
                continue
            text = (entry / "smaps_rollup").read_text()
        except OSError:
            continue
        m = re.search(r"^Pss:\s+(\d+) kB", text, re.M)
        total += int(m.group(1)) if m else 0
    return total


def postgres_sizes(psql: list[str], schema: str) -> tuple[int, int]:
    """(bytes of the store's schema, bytes of the server's write-ahead log)."""
    sql = ("SELECT coalesce(sum(pg_total_relation_size(c.oid)),0) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace "
           f"WHERE n.nspname='{schema}' AND c.relkind IN ('r','m'); SELECT coalesce(sum(size),0) FROM pg_ls_waldir();")
    try:
        out = subprocess.run(psql + ["-tA", "-c", sql], capture_output=True, text=True, timeout=30).stdout.split()
        return int(out[0]), int(out[1])
    except (subprocess.SubprocessError, ValueError, IndexError):
        return 0, 0


def postgres_wal_stats(psql: list[str]) -> dict:
    """The server's WAL counters (pg_stat_wal): records, bytes, writes and
    syncs (its fsyncs of the WAL)."""
    sql = "SELECT wal_records, wal_bytes, wal_write, wal_sync FROM pg_stat_wal"
    try:
        out = subprocess.run(psql + ["-tA", "-F", " ", "-c", sql], capture_output=True, text=True, timeout=30).stdout.split()
        return dict(zip(["wal_records", "wal_bytes", "wal_write", "wal_sync"], map(int, out)))
    except (subprocess.SubprocessError, ValueError):
        return {}


def file_size(path: Path) -> int:
    try:
        return path.stat().st_size
    except OSError:
        return 0


def pct(values: list[float], q: float) -> float | None:
    if not values:
        return None
    values = sorted(values)
    k = (len(values) - 1) * q
    lo, hi = int(k), min(int(k) + 1, len(values) - 1)
    return values[lo] + (values[hi] - values[lo]) * (k - lo)


class Server:
    def __init__(self, base: str, token: str, workspace: str):
        self.base, self.token, self.workspace = base, token, workspace

    def post(self, path: str, body: dict, session: str | None = None, timeout: float = 600) -> tuple[int, dict, dict]:
        data = json.dumps(body).encode()
        request = urllib.request.Request(self.base + path, data=data, method="POST")
        request.add_header("Content-Type", "application/json")
        request.add_header("Accept", "application/json, text/event-stream")
        request.add_header("Authorization", f"Bearer {self.token}")
        if session:
            request.add_header("Mcp-Session-Id", session)
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                raw = response.read()
                headers = dict(response.headers)
                status = response.status
        except urllib.error.HTTPError as error:
            raw, headers, status = error.read(), dict(error.headers), error.code
        try:
            parsed = json.loads(raw) if raw else {}
        except json.JSONDecodeError:
            parsed = {"raw": raw[:500].decode(errors="replace")}
        return status, parsed, headers

    def mcp(self, repository: str, method: str, params: dict, session: str | None, ident: int):
        return self.post(f"/mcp/{self.workspace}/{repository}",
                         {"jsonrpc": "2.0", "id": ident, "method": method, "params": params}, session)

    def command(self, operation: str, arguments: dict) -> dict:
        status, body, _ = self.post(f"/api/workspaces/{self.workspace}/repositories/command",
                                    {"operation": operation, "arguments": arguments})
        if status >= 300:
            raise RuntimeError(f"{operation}: HTTP {status}: {body}")
        return body


class Corpus:
    """Arguments for the mix, drawn from the seed checkout."""

    WORDS = re.compile(r"^func (?:\([^)]*\) )?([A-Z][A-Za-z0-9_]{3,40})\(", re.M)

    def __init__(self, root: Path, rng: random.Random, limit: int = 4000):
        files = subprocess.run(["git", "-C", str(root), "ls-files"], capture_output=True, text=True, check=True).stdout.split()
        code = [f for f in files if f.endswith((".go", ".rs", ".py", ".ts", ".c", ".h", ".java"))]
        rng.shuffle(code)
        self.files = code[:limit] or files[:limit]
        symbols: set[str] = set()
        for path in self.files[:1500]:
            try:
                text = (root / path).read_text(errors="replace")
            except OSError:
                continue
            for match in self.WORDS.finditer(text):
                symbols.add(match.group(1))
        self.symbols = sorted(symbols) or ["main"]
        self.rng = rng

    def args(self, kind: str, schema: dict) -> dict:
        r, props = self.rng, set((schema.get("properties") or {}).keys())
        def put(out: dict, names: tuple[str, ...], value):
            for name in names:
                if name in props:
                    out[name] = value
                    return
        out: dict = {}
        symbol = r.choice(self.symbols)
        if kind == "query":
            put(out, ("query", "q", "text", "pattern", "question"), symbol)
        elif kind == "symbol":
            put(out, ("symbol", "node", "name", "id", "query", "label"), symbol)
        elif kind == "pair":
            put(out, ("source", "from", "start"), symbol)
            put(out, ("target", "to", "end"), r.choice(self.symbols))
        elif kind == "file":
            put(out, ("path", "file", "file_path"), r.choice(self.files))
        elif kind == "prefix":
            path = r.choice(self.files)
            put(out, ("prefix",), path.rsplit("/", 1)[0] if "/" in path else "")
            put(out, ("limit",), 50)
        elif kind == "question":
            put(out, ("question", "query", "task", "text"), f"How does {symbol} work and what calls it?")
        for name in (schema.get("required") or []):
            if name not in out:
                out[name] = symbol
        return out


def build_mix(tools: list[dict]) -> tuple[list[tuple[str, int, str, dict]], list[str]]:
    listed = {t["name"]: t for t in tools}
    mix, dropped = [], []
    for name, weight, kind in MIX:
        if name in listed:
            mix.append((name, weight, kind, listed[name].get("inputSchema") or {}))
        else:
            dropped.append(name)
    return mix, dropped


class Readers:
    def __init__(self, server: Server, repository: str, corpus: Corpus, mix, clients: int,
                 calls_per_pass: int, seed: int):
        self.server, self.repository, self.corpus, self.mix = server, repository, corpus, mix
        self.clients, self.calls_per_pass, self.seed = clients, calls_per_pass, seed
        self.stop = threading.Event()
        self.lock = threading.Lock()
        self.calls: list[tuple[float, float, str, int, bool, str]] = []  # t, seconds, tool, status, ok, phase
        self.phase = "warmup"
        self.passes = 0
        self.errors: dict[str, str] = {}  # the first error text of each tool and status
        self.threads: list[threading.Thread] = []

    def start(self):
        for n in range(self.clients):
            t = threading.Thread(target=self.client, args=(n,), daemon=True)
            t.start()
            self.threads.append(t)

    def open_session(self, ident: int) -> str | None:
        status, body, headers = self.server.mcp(self.repository, "initialize", {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "store-load", "version": "1"}}, None, ident)
        session = headers.get("Mcp-Session-Id") or headers.get("mcp-session-id")
        if session:
            self.server.mcp(self.repository, "notifications/initialized", {}, session, ident + 1)
        with self.lock:
            self.passes += 1
        return session

    def client(self, n: int):
        rng = random.Random(self.seed * 1000 + n)
        weights = [w for _, w, _, _ in self.mix]
        ident, made, session = 0, 0, None
        while not self.stop.is_set():
            if made % self.calls_per_pass == 0:
                ident += 2
                try:
                    session = self.open_session(ident)
                except Exception:  # a refused or dropped connection is a failed call
                    session = None
            name, _, kind, schema = rng.choices(self.mix, weights=weights)[0]
            arguments = self.corpus.args(kind, schema)
            ident += 1
            started = now()
            try:
                status, body, _ = self.server.mcp(self.repository, "tools/call",
                                                  {"name": name, "arguments": arguments}, session, ident)
                ok = status == 200 and "error" not in body and not (body.get("result") or {}).get("isError")
                if not ok:
                    key = f"{name}:{status}"
                    text = json.dumps(body.get("error") or body.get("result") or body)[:400]
                    with self.lock:
                        self.errors.setdefault(key, text)
            except Exception as error:
                status, ok = 0, False
                with self.lock:
                    self.errors.setdefault(f"{name}:0", str(error)[:400])
            elapsed = now() - started
            made += 1
            with self.lock:
                self.calls.append((started, elapsed, name, status, ok, self.phase))

    def finish(self):
        self.stop.set()
        for t in self.threads:
            t.join(timeout=700)


def run_logged(cmd: list[str], log: Path, env: dict, cwd: Path | None = None) -> subprocess.CompletedProcess:
    with open(log, "wb") as out:
        return subprocess.run(cmd, stdout=out, stderr=subprocess.STDOUT, env=env, cwd=cwd)


def strace_wrap(cmd: list[str], out: Path, enabled: bool) -> list[str]:
    if not enabled:
        return cmd
    return ["strace", "-f", "--seccomp-bpf", "-qq", "-c", "-e",
            "trace=fsync,fdatasync,sync_file_range", "-o", str(out)] + cmd


def fsync_count(path: Path) -> dict:
    """strace -c's table: {syscall: calls}."""
    counts: dict[str, int] = {}
    try:
        text = path.read_text()
    except OSError:
        return counts
    for line in text.splitlines():
        parts = line.split()
        if len(parts) >= 5 and parts[-1] in ("fsync", "fdatasync", "sync_file_range"):
            # % time, seconds, usecs/call, calls, [errors], syscall
            counts[parts[-1]] = int(parts[3])
    return counts


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin", required=True, type=Path, help="the graphicle binary")
    ap.add_argument("--data-dir", required=True, type=Path, help="data folder; emptied first")
    ap.add_argument("--seed", required=True, type=Path, help="checkout synced before the load, which readers query")
    ap.add_argument("--index", required=True, type=Path, help="checkout indexed under load")
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument("--label", default="", help="free text recorded with the run (device, backend)")
    ap.add_argument("--clients", type=int, default=33)
    ap.add_argument("--calls-per-pass", type=int, default=20)
    ap.add_argument("--warmup", type=float, default=60)
    ap.add_argument("--after", type=float, default=120)
    ap.add_argument("--sample", type=float, default=10)
    ap.add_argument("--index-limit", type=float, default=8 * 3600, help="seconds before the indexer is killed (the gate's limit)")
    ap.add_argument("--cgroup", type=Path, help="cgroup (v1 blkio or v2) directory both processes join")
    ap.add_argument("--no-strace", action="store_true")
    ap.add_argument("--keep-seed", action="store_true", help="reuse --data-dir if it already holds the seed")
    ap.add_argument("--env", action="append", default=[], help="NAME=VALUE for both processes")
    ap.add_argument("--rng", type=int, default=1)
    ap.add_argument("--psql", default="", help="psql command (space-separated) to reach the store's PostgreSQL server; "
                    "store and WAL sizes are then the schema's and the server's")
    a = ap.parse_args()

    a.out.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ)
    env["GRAPHICLE_NO_UPDATE_NOTIFIER"] = "1"
    env["GRAPHICLE_NO_BROWSER"] = "1"
    for item in a.env:
        name, _, value = item.partition("=")
        env[name] = value
    data = a.data_dir.resolve()
    database = data / "reviews.sqlite3"
    wal = data / "reviews.sqlite3-wal"
    shm = data / "reviews.sqlite3-shm"

    def join_cgroup():
        if a.cgroup:
            (a.cgroup / "cgroup.procs").write_text(str(os.getpid()))
    # Children inherit the cgroup; the harness itself does little IO.
    pre = join_cgroup if a.cgroup else None

    record: dict = {"label": a.label, "argv": sys.argv, "clients": a.clients, "env": a.env,
                    "started_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
    record["version"] = subprocess.run([str(a.bin), "--version"], capture_output=True, text=True).stdout.strip()

    seeded = a.keep_seed and (data / "seed.json").is_file()
    if not seeded:
        shutil.rmtree(data, ignore_errors=True)
        data.mkdir(parents=True)
        t = now()
        result = subprocess.run([str(a.bin), "--data-dir", str(data), "sync", str(a.seed.resolve()), "--json"],
                                capture_output=True, env=env, preexec_fn=pre)
        (a.out / "seed-sync.log").write_bytes(result.stderr)
        if result.returncode != 0:
            print(result.stderr.decode(errors="replace")[-3000:], file=sys.stderr)
            return 1
        seed = json.loads(result.stdout)
        seed["seconds"] = now() - t
        (data / "seed.json").write_text(json.dumps(seed))
    seed = json.loads((data / "seed.json").read_text())
    record["seed"] = {"repository_id": seed.get("repository_id"), "seconds": seed.get("seconds"),
                      "graph": {k: (seed.get("graph") or {}).get(k) for k in ("status", "nodes", "edges")}}
    session = json.loads((data / "local-session.json").read_text())
    workspace, token = session["workspace"], session["capability"]

    port = free_port()
    server_cmd = strace_wrap([str(a.bin), "--data-dir", str(data), "serve", "--port", str(port)],
                             a.out / "server.strace", not a.no_strace)
    server_log = open(a.out / "server.log", "wb")
    server = subprocess.Popen(server_cmd, stdout=server_log, stderr=subprocess.STDOUT, env=env, preexec_fn=pre)
    client = Server(f"http://127.0.0.1:{port}", token, workspace)
    deadline = now() + 120
    while now() < deadline:
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=2) as r:
                if r.status == 200:
                    break
        except Exception:
            time.sleep(0.5)
    else:
        print("server did not start", file=sys.stderr)
        server.kill()
        return 1

    repository = seed["repository_id"]
    status, listed, headers = client.mcp(repository, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                          "clientInfo": {"name": "store-load", "version": "1"}}, None, 1)
    sid = headers.get("Mcp-Session-Id") or headers.get("mcp-session-id")
    status, listed, _ = client.mcp(repository, "tools/list", {}, sid, 2)
    tools = (listed.get("result") or {}).get("tools") or []
    (a.out / "tools.json").write_text(json.dumps(tools, indent=1))
    mix, dropped = build_mix(tools)
    record["mix"] = [[n, w] for n, w, _, _ in mix]
    record["mix_dropped"] = dropped
    if not mix:
        print(f"none of the mix's tools are listed: {[t['name'] for t in tools]}", file=sys.stderr)
        server.kill()
        return 1

    corpus = Corpus(a.seed.resolve(), random.Random(a.rng))
    readers = Readers(client, repository, corpus, mix, a.clients, a.calls_per_pass, a.rng)

    samples: list[dict] = []
    indexer: dict = {"pid": None, "peak": 0}
    server_peak = [0]
    t0 = now()
    sampling = threading.Event()

    psql = a.psql.split() if a.psql else []
    schema = ""
    if psql:
        schema = json.loads((data / "postgres.json").read_text())["schema"]
    pg_peak = [0, 0]

    def sample_once():
        spid = graphicle_pid(server.pid)
        ipid = graphicle_pid(indexer["pid"])
        s_rss, s_hwm = rss_kib(spid)
        i_rss, i_hwm = rss_kib(ipid)
        server_peak[0] = max(server_peak[0], s_hwm)
        indexer["peak"] = max(indexer["peak"], i_hwm)
        with readers.lock:
            done = len(readers.calls)
            failed = sum(1 for c in readers.calls if not c[4])
            phase = readers.phase
        if psql:
            store_bytes, wal_bytes = postgres_sizes(psql, schema)
            shm_bytes = 0
            pg_rss, pg_pss = postgres_rss_kib(), postgres_pss_kib()
            pg_peak[0], pg_peak[1] = max(pg_peak[0], pg_rss), max(pg_peak[1], pg_pss)
        else:
            store_bytes, wal_bytes, shm_bytes = file_size(database), file_size(wal), file_size(shm)
            pg_rss = pg_pss = 0
        samples.append({"t": round(now() - t0, 1), "phase": phase, "store_bytes": store_bytes,
                        "wal_bytes": wal_bytes, "shm_bytes": shm_bytes,
                        "server_rss_kib": s_rss, "indexer_rss_kib": i_rss, "postgres_rss_kib": pg_rss,
                        "postgres_pss_kib": pg_pss, "reads_done": done, "reads_failed": failed})

    def sampler():
        while not sampling.is_set():
            sample_once()
            sampling.wait(a.sample)

    wal_before = postgres_wal_stats(psql) if psql else {}
    sampler_thread = threading.Thread(target=sampler, daemon=True)
    sampler_thread.start()
    readers.start()
    time.sleep(a.warmup)

    # The gate's flow: register the project empty, then index it.
    name = a.index.resolve().name
    registered = client.command("register", {"project_key": f"bench-{name}-{int(time.time())}", "name": name})
    project = registered["id"]
    readers.phase = "indexing"
    started = now()
    index_log = a.out / "indexer.log"
    killed = False
    cmd = strace_wrap([str(a.bin), "--data-dir", str(data), "sync", str(a.index.resolve()), "--project", project, "--json"],
                      a.out / "indexer.strace", not a.no_strace)
    with open(index_log, "wb") as log_file, open(a.out / "indexer.json", "wb") as out_file:
        proc = subprocess.Popen(cmd, stdout=out_file, stderr=log_file, env=env, preexec_fn=pre)
        indexer["pid"] = proc.pid
        try:
            code = proc.wait(timeout=a.index_limit)
        except subprocess.TimeoutExpired:
            proc.send_signal(signal.SIGKILL)
            code = proc.wait()
            killed = True
    index_seconds = now() - started
    readers.phase = "after"
    time.sleep(a.after)
    readers.finish()
    sample_once()
    sampling.set()
    sampler_thread.join()

    # Stop the server itself (under strace, its child): strace then exits
    # with it and writes its counts.
    target = graphicle_pid(server.pid)
    if target and target != server.pid:
        os.kill(target, signal.SIGINT)
    else:
        server.send_signal(signal.SIGINT)
    try:
        server.wait(timeout=60)
    except subprocess.TimeoutExpired:
        server.kill()
        server.wait()
    server_log.close()
    record["after_shutdown"] = {"store_bytes": file_size(database), "wal_bytes": file_size(wal)}

    # Summaries.
    calls = readers.calls
    by_phase: dict = {}
    for phase in ("warmup", "indexing", "after"):
        rows = [c for c in calls if c[5] == phase]
        lat = [c[1] for c in rows if c[4]]
        span = (max(c[0] + c[1] for c in rows) - min(c[0] for c in rows)) if rows else 0
        by_phase[phase] = {"calls": len(rows), "failed": sum(1 for c in rows if not c[4]),
                           "p50_ms": _percentile_ms(lat, .5), "p95_ms": _percentile_ms(lat, .95),
                           "p99_ms": _percentile_ms(lat, .99),
                           "reads_per_s": round(len(rows) / span, 1) if span else 0}
    tools_summary = {}
    for name in sorted({c[2] for c in calls}):
        lat = [c[1] for c in calls if c[2] == name and c[4]]
        tools_summary[name] = {"calls": sum(1 for c in calls if c[2] == name),
                               "failed": sum(1 for c in calls if c[2] == name and not c[4]),
                               "p50_ms": _percentile_ms(lat, .5), "p99_ms": _percentile_ms(lat, .99)}
    failures = {}
    for c in calls:
        if not c[4]:
            failures[f"{c[2]}:{c[3]}"] = failures.get(f"{c[2]}:{c[3]}", 0) + 1
    record.update({
        "index": {"project": project, "exit": code, "killed_at_limit": killed, "seconds": round(index_seconds, 1),
                  "result": _tail_json(a.out / "indexer.json")},
        "reads": by_phase, "tools": tools_summary, "failures": failures, "errors": readers.errors, "passes": readers.passes,
        "peak_rss_mib": {"server": round(server_peak[0] / 1024), "indexer": round(indexer["peak"] / 1024),
                         "postgres_rss": round(pg_peak[0] / 1024), "postgres_pss": round(pg_peak[1] / 1024)},
        "wal_peak_mib": round(max(s["wal_bytes"] for s in samples) / 2**20, 1),
        "wal_end_mib": round(samples[-1]["wal_bytes"] / 2**20, 1),
        "store_end_mib": round(samples[-1]["store_bytes"] / 2**20, 1),
        "fsyncs": {"server": fsync_count(a.out / "server.strace"), "indexer": fsync_count(a.out / "indexer.strace")},
        "postgres_wal": ({k: v - wal_before.get(k, 0) for k, v in postgres_wal_stats(psql).items()} if psql else None),
    })
    with open(a.out / "samples.csv", "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=list(samples[0].keys()))
        w.writeheader()
        w.writerows(samples)
    with open(a.out / "calls.csv", "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["t", "seconds", "tool", "status", "ok", "phase"])
        for c in calls:
            w.writerow([round(c[0] - t0, 3), round(c[1], 4), c[2], c[3], int(c[4]), c[5]])
    (a.out / "run.json").write_text(json.dumps(record, indent=1, allow_nan=False))
    print(json.dumps({k: record[k] for k in ("label", "index", "reads", "peak_rss_mib", "wal_peak_mib", "wal_end_mib",
                                             "store_end_mib", "fsyncs", "failures")}, indent=1))
    return 0


def _tail_json(path: Path):
    try:
        value = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError):
        return None
    if isinstance(value, dict):
        graph = value.get("graph") or {}
        return {"status": graph.get("status") or value.get("state"), "nodes": graph.get("nodes"),
                "edges": graph.get("edges"), "files": value.get("files")}
    return None


def _percentile_ms(values: list[float], q: float) -> float | None:
    value = pct(values, q)
    return None if value is None else round(value * 1000, 1)


if __name__ == "__main__":
    sys.exit(main())
