#!/usr/bin/env python3
"""SWE-bench Pro (public set, V2: 642 tasks) as a change-localization benchmark.

usage:
  data.py fetch --out DIR
      downloads the pinned dataset file (Hugging Face ScaleAI/SWE-bench_Pro, config "default", split
      "test", revision REVISION) into DIR and checks its sha256
  data.py tasks --parquet FILE --tasks OUT.jsonl --texts DIR
      writes one line per task to OUT.jsonl and each task's problem statement to DIR/<instance_id>.txt
      (the texts are not committed; `fetch` and `tasks` rebuild them and text_sha256 checks them)

A task's line: instance_id, repo, base_commit, language, gold, added, text_sha256, names_gold_path, hard.
- gold: the files the reference patch (the dataset's `patch`, without the test patch) changes and that
  exist at the base commit: modified, deleted, or the old side of a rename. These are what a context can
  hold; e1.py checks that each one exists at the base commit.
- added: the files the reference patch creates. No context can hold them, so they are not gold.
- names_gold_path: the problem statement names a gold file by a path of at least two parts.
The question text is the problem statement alone, as the dataset gives it; the dataset's `requirements`
and `interface` fields are not used, since they often name the files and functions to change.
"""
import argparse
import hashlib
import json
import os
import sys
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "whole-repo"))
from mine import names_gold_path, parse_diff  # noqa: E402

DATASET = "ScaleAI/SWE-bench_Pro"
REVISION = "2d52cb3df914a3fcf80c7f66738b3a88ae37fc50"  # V2 (2026-09-22), 642 tasks
FILE = "data/default/test-00000-of-00001.parquet"
FILE_SHA256 = "1067f72c1f43bbc578276888625db3b836cebb535546c9eaf824c5d5dd104a7f"


def sha256_bytes(b):
    return hashlib.sha256(b).hexdigest()


def cmd_fetch(a):
    os.makedirs(a.out, exist_ok=True)
    url = f"https://huggingface.co/datasets/{DATASET}/resolve/{REVISION}/{FILE}"
    dest = os.path.join(a.out, os.path.basename(FILE))
    with urllib.request.urlopen(url, timeout=120) as r:
        data = r.read()
    got = sha256_bytes(data)
    if got != FILE_SHA256:
        raise SystemExit(f"sha256 mismatch: {got} (expected {FILE_SHA256})")
    with open(dest, "wb") as f:
        f.write(data)
    print(f"{dest}: {len(data)} bytes, sha256 ok")


def gold_of(patch):
    gold, added = [], []
    for s in parse_diff(patch):
        if s["status"] == "added":
            if s["new"] and s["new"] not in added:
                added.append(s["new"])
        elif s["old"] and s["old"] not in gold:
            gold.append(s["old"])
    return gold, added


def cmd_tasks(a):
    import pyarrow.parquet as pq
    with open(a.parquet, "rb") as f:
        if sha256_bytes(f.read()) != FILE_SHA256:
            raise SystemExit("the parquet file is not the pinned revision")
    rows = pq.read_table(a.parquet).to_pylist()
    os.makedirs(a.texts, exist_ok=True)
    out = []
    for r in sorted(rows, key=lambda r: r["instance_id"]):
        text = r["problem_statement"] or ""
        gold, added = gold_of(r["patch"] or "")
        with open(os.path.join(a.texts, r["instance_id"] + ".txt"), "w", encoding="utf-8") as f:
            f.write(text)
        out.append({"instance_id": r["instance_id"], "repo": r["repo"], "base_commit": r["base_commit"],
                    "language": r["repo_language"], "gold": gold, "added": added,
                    "text_sha256": sha256_bytes(text.encode("utf-8")),
                    "names_gold_path": names_gold_path(text, gold), "hard": bool(r.get("hard"))})
    with open(a.tasks, "w") as f:
        for t in out:
            f.write(json.dumps(t, sort_keys=True) + "\n")
    by_repo = {}
    for t in out:
        d = by_repo.setdefault(t["repo"], {"tasks": 0, "commits": set(), "no_gold": 0, "gold": 0, "added": 0,
                                           "named": 0, "lang": set()})
        d["tasks"] += 1
        d["commits"].add(t["base_commit"])
        d["no_gold"] += int(not t["gold"])
        d["gold"] += len(t["gold"])
        d["added"] += len(t["added"])
        d["named"] += int(t["names_gold_path"])
        d["lang"].add(t["language"])
    print("repo\ttasks\tbase_commits\tgold_files\tadded_files\tno_gold\tnames_gold\tlanguages")
    for k in sorted(by_repo):
        d = by_repo[k]
        print(f"{k}\t{d['tasks']}\t{len(d['commits'])}\t{d['gold']}\t{d['added']}\t{d['no_gold']}\t{d['named']}\t"
              f"{','.join(sorted(d['lang']))}")
    print(f"total\t{len(out)}")


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    f = sub.add_parser("fetch")
    f.add_argument("--out", required=True)
    t = sub.add_parser("tasks")
    t.add_argument("--parquet", required=True)
    t.add_argument("--tasks", required=True)
    t.add_argument("--texts", required=True)
    a = ap.parse_args()
    {"fetch": cmd_fetch, "tasks": cmd_tasks}[a.cmd](a)


if __name__ == "__main__":
    main()
