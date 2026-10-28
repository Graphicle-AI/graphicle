#!/usr/bin/env python3
"""Build Graphicle's index of each checked-out repository and record its time and size.

usage: index.py --binary GRAPHICLE --work WORKDIR [NAME ...]

Runs `graphicle sync WORKDIR/src/NAME --data-dir WORKDIR/data/NAME --json` for each repository
(each in its own data folder, so the size is that repository's alone) and writes
WORKDIR/index/NAME.json: wall seconds, CPU seconds, peak memory of the sync process, the data
folder's size in bytes and the binary's version. A repository whose result file exists is skipped.
"""
import argparse
import json
import os
import resource
import subprocess
import time


def du_bytes(path):
    total = 0
    for root, _dirs, files in os.walk(path):
        for f in files:
            try:
                total += os.lstat(os.path.join(root, f)).st_size
            except OSError:
                pass
    return total


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", required=True)
    ap.add_argument("--work", required=True)
    ap.add_argument("names", nargs="*")
    a = ap.parse_args()
    work = os.path.abspath(a.work)
    names = a.names or sorted(os.listdir(os.path.join(work, "src")))
    os.makedirs(os.path.join(work, "index"), exist_ok=True)
    version = subprocess.run([a.binary, "--version"], text=True, capture_output=True).stdout.strip()
    for name in names:
        out = os.path.join(work, "index", name + ".json")
        if os.path.exists(out):
            print(f"{name}: indexed already", flush=True)
            continue
        src = os.path.join(work, "src", name)
        data = os.path.join(work, "data", name)
        os.makedirs(data, exist_ok=True)
        log = open(os.path.join(work, "index", name + ".log"), "w")
        before = resource.getrusage(resource.RUSAGE_CHILDREN)
        t0 = time.time()
        r = subprocess.run([a.binary, "sync", src, "--data-dir", data, "--json"],
                           stdout=subprocess.PIPE, stderr=log, text=True)
        wall = time.time() - t0
        after = resource.getrusage(resource.RUSAGE_CHILDREN)
        log.close()
        try:
            summary = json.loads(r.stdout) if r.stdout.strip() else {}
        except json.JSONDecodeError:
            summary = {"unparsed_stdout_bytes": len(r.stdout)}
        row = {
            "name": name,
            "graphicle": version,
            "exit": r.returncode,
            "wall_seconds": round(wall, 1),
            "cpu_seconds": round((after.ru_utime - before.ru_utime) + (after.ru_stime - before.ru_stime), 1),
            "peak_rss_mib": round(after.ru_maxrss / 1024, 1),
            "data_bytes": du_bytes(data),
            "sync": summary,
        }
        with open(out if r.returncode == 0 else out + ".failed", "w") as f:
            json.dump(row, f, indent=1, sort_keys=True)
        print(f"{name}: exit {r.returncode}, {wall:.0f} s, {row['data_bytes'] / 2**20:.0f} MiB", flush=True)


if __name__ == "__main__":
    main()
