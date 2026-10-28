#!/usr/bin/env python3
"""Pin each repository at its snapshot commit S and check S out.

usage: checkout.py WORKDIR [NAME ...]

For each repository in repos.json (or only the NAMEs given):
1. a commit-only clone of the default branch (git --filter=tree:0) in WORKDIR/hist/NAME.git;
2. S: the commit recorded in snapshots.json, else the last commit on the default branch's
   first-parent line whose committer time is at or before repos.json's snapshot_cutoff;
3. a depth-1 fetch of S into WORKDIR/src/NAME, checked out detached.

Writes WORKDIR/snapshots/NAME.json. A resolved S that snapshots.json does not record yet is
printed, so it can be added there; a recorded S is checked against the history.
Every step is skipped when its result is already there, so the script can be run again.
"""
import datetime
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))


def git(*args, cwd=None, capture=True):
    r = subprocess.run(["git", *args], cwd=cwd, text=True,
                       stdout=subprocess.PIPE if capture else None, stderr=subprocess.PIPE)
    if r.returncode != 0:
        raise SystemExit(f"git {' '.join(args)} failed: {r.stderr.strip()[:400]}")
    return (r.stdout or "").strip()


def main():
    if len(sys.argv) < 2:
        raise SystemExit(__doc__)
    work = os.path.abspath(sys.argv[1])
    only = set(sys.argv[2:])
    cfg = json.load(open(os.path.join(HERE, "repos.json")))
    rec_path = os.path.join(HERE, "snapshots.json")
    recorded = json.load(open(rec_path)) if os.path.exists(rec_path) else {}
    os.makedirs(os.path.join(work, "snapshots"), exist_ok=True)
    os.makedirs(os.path.join(work, "hist"), exist_ok=True)
    os.makedirs(os.path.join(work, "src"), exist_ok=True)
    for r in cfg["repos"]:
        name, slug, branch = r["name"], r["slug"], r["branch"]
        if only and name not in only:
            continue
        url = f"https://github.com/{slug}.git"
        hist = os.path.join(work, "hist", name + ".git")
        if not os.path.exists(os.path.join(hist, "HEAD")):
            git("clone", "-q", "--bare", "--filter=tree:0", "--single-branch", "--branch", branch, url, hist)
        resolved = git("rev-list", "-1", "--first-parent", f"--before={cfg['snapshot_cutoff']}", branch, cwd=hist)
        s = recorded.get(name, {}).get("sha") or resolved
        if s != resolved:
            print(f"{name}: recorded S {s[:12]} differs from the resolved {resolved[:12]}; keeping the recorded one",
                  flush=True)
        ct = int(git("show", "-s", "--format=%ct", s, cwd=hist))
        committed = datetime.datetime.fromtimestamp(ct, datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        src = os.path.join(work, "src", name)
        head = ""
        if os.path.exists(os.path.join(src, ".git")):
            try:
                head = git("rev-parse", "HEAD", cwd=src)
            except SystemExit:
                head = ""
        if head != s:
            os.makedirs(src, exist_ok=True)
            if not os.path.exists(os.path.join(src, ".git")):
                git("init", "-q", src)
                git("remote", "add", "origin", url, cwd=src)
            git("fetch", "-q", "--depth", "1", "origin", s, cwd=src)
            git("checkout", "-q", "--detach", "FETCH_HEAD", cwd=src)
        files = len(git("ls-files", "-z", cwd=src).split("\0")) - 1
        row = {"slug": slug, "branch": branch, "sha": s, "committed": committed, "tracked_files": files,
               "resolved": resolved}
        if name not in recorded:
            print(f"{name}: S {s} committed {committed} (not yet in snapshots.json)", flush=True)
        else:
            print(f"{name}: S {s[:12]} committed {committed}, {files} tracked files", flush=True)
        with open(os.path.join(work, "snapshots", name + ".json"), "w") as f:
            json.dump(row, f, indent=1, sort_keys=True)


if __name__ == "__main__":
    main()
