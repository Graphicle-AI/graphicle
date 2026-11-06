#!/usr/bin/env python3
"""The WAL growth of the 2026-11-03 outage, without Graphicle.

One writer process commits batches of rows (as indexing does) while reader
threads in another process run short read transactions back to back (as MCP
tool calls do). With SQLite's defaults the automatic checkpoint is PASSIVE:
it copies back only the frames no reader still needs, and the WAL is reset to
its start only when a checkpoint has copied everything *and* no reader is
using the WAL. With readers always in flight that moment rarely comes, so the
file grows for as long as the writer writes.

  --mode default    SQLite's defaults (what Graphicle 0.1.12 runs)
  --mode restart    the writer runs `PRAGMA wal_checkpoint(RESTART)` once the
                    WAL passes --limit-mib, with journal_size_limit set

Stdlib only. Prints one JSON line per second, then a summary.
"""
import argparse
import json
import multiprocessing as mp
import os
import sqlite3
import threading
import time


def writer(path, seconds, batch, mode, limit, out):
    db = sqlite3.connect(path, isolation_level=None, timeout=10)
    db.execute("PRAGMA journal_mode=WAL")
    if mode == "restart":
        db.execute(f"PRAGMA journal_size_limit={limit}")
    payload = os.urandom(300).hex()
    end = time.monotonic() + seconds
    commits = checkpoints = busy = 0
    while time.monotonic() < end:
        db.execute("BEGIN IMMEDIATE")
        db.executemany("INSERT INTO t(k, v) VALUES(?, ?)", ((os.urandom(8).hex(), payload) for _ in range(batch)))
        db.execute("COMMIT")
        commits += 1
        if mode == "restart" and os.path.getsize(path + "-wal") > limit:
            row = db.execute("PRAGMA wal_checkpoint(RESTART)").fetchone()
            checkpoints += 1
            busy += row[0]
    out.put({"commits": commits, "checkpoints": checkpoints, "busy": busy})


def readers(path, seconds, clients, read_ms, out):
    stop = time.monotonic() + seconds
    done = [0] * clients
    lat = []
    lock = threading.Lock()

    def one(n):
        db = sqlite3.connect(path, isolation_level=None, timeout=10)
        while time.monotonic() < stop:
            t = time.monotonic()
            db.execute("BEGIN")
            db.execute("SELECT count(*) FROM (SELECT * FROM t ORDER BY rowid DESC LIMIT 2000)").fetchone()
            time.sleep(read_ms / 1000)  # the rest of a tool call, inside its snapshot
            db.execute("COMMIT")
            with lock:
                lat.append(time.monotonic() - t)
            done[n] += 1

    threads = [threading.Thread(target=one, args=(n,)) for n in range(clients)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    lat.sort()
    out.put({"reads": sum(done), "p50_ms": lat[len(lat) // 2] * 1000, "p99_ms": lat[int(len(lat) * .99)] * 1000})


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--path", required=True)
    ap.add_argument("--mode", choices=["default", "restart"], default="default")
    ap.add_argument("--seconds", type=float, default=60)
    ap.add_argument("--clients", type=int, default=33)
    ap.add_argument("--read-ms", type=float, default=20)
    ap.add_argument("--batch", type=int, default=500)
    ap.add_argument("--limit-mib", type=int, default=64)
    a = ap.parse_args()
    for suffix in ("", "-wal", "-shm"):
        try:
            os.remove(a.path + suffix)
        except FileNotFoundError:
            pass
    db = sqlite3.connect(a.path, isolation_level=None)
    db.execute("PRAGMA journal_mode=WAL")
    db.execute("CREATE TABLE t(k TEXT, v TEXT)")
    db.execute("CREATE INDEX t_k ON t(k)")
    q = mp.Queue()
    limit = a.limit_mib * 2**20
    w = mp.Process(target=writer, args=(a.path, a.seconds, a.batch, a.mode, limit, q))
    r = mp.Process(target=readers, args=(a.path, a.seconds, a.clients, a.read_ms, q))
    w.start(), r.start()
    peak = 0
    t0 = time.monotonic()
    while w.is_alive() or r.is_alive():
        size = os.path.getsize(a.path + "-wal") if os.path.exists(a.path + "-wal") else 0
        peak = max(peak, size)
        print(json.dumps({"t": round(time.monotonic() - t0), "wal_mib": round(size / 2**20, 1)}), flush=True)
        time.sleep(1)
    w.join(), r.join()
    results = [q.get(), q.get()]
    db.close()
    summary = {"mode": a.mode, "sqlite": sqlite3.sqlite_version, "wal_peak_mib": round(peak / 2**20, 1),
               "db_mib": round(os.path.getsize(a.path) / 2**20, 1)}
    for r in results:
        summary.update(r)
    print(json.dumps(summary))


if __name__ == "__main__":
    main()
