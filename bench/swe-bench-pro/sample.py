#!/usr/bin/env python3
"""The answer-run sample: 100 SWE-bench Pro tasks, stratified by repository.

usage: sample.py --tasks tasks.jsonl --out sample.json [--n 100] [--seed 20261027]

- Population: the tasks with at least one gold file (data.py).
- Allocation: proportional to each repository's share of the population, by largest remainder (ties
  broken by repository name), with at least one task per repository (allocate()).
- Draw: for the i-th repository in name order, random.Random(seed + i).sample(its task ids in sorted
  order, k).
"""
import argparse
import json
import random


def allocate(counts, n):
    """Largest remainder: each repository gets the whole part of its quota, the remaining places go to the
    largest fractional parts (ties by name); then a repository left with none takes one from the largest
    allocation (ties by name)."""
    total = sum(counts.values())
    quota = {r: n * c / total for r, c in counts.items()}
    alloc = {r: int(q) for r, q in quota.items()}
    left = n - sum(alloc.values())
    for r in sorted(counts, key=lambda r: (-(quota[r] - int(quota[r])), r))[:left]:
        alloc[r] += 1
    for r in sorted(counts):
        if alloc[r] == 0:
            big = min(alloc, key=lambda k: (-alloc[k], k))
            alloc[big] -= 1
            alloc[r] = 1
    return alloc


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tasks", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--n", type=int, default=100)
    ap.add_argument("--seed", type=int, default=20261027)
    a = ap.parse_args()
    tasks = [json.loads(l) for l in open(a.tasks) if l.strip()]
    by_repo = {}
    for t in tasks:
        if t["gold"]:
            by_repo.setdefault(t["repo"], []).append(t["instance_id"])
    counts = {r: len(v) for r, v in by_repo.items()}
    alloc = allocate(counts, a.n)
    ids = []
    for i, r in enumerate(sorted(by_repo)):
        ids += random.Random(a.seed + i).sample(sorted(by_repo[r]), alloc[r])
    out = {"seed": a.seed, "n": len(ids), "population": sum(counts.values()),
           "excluded_no_gold": sum(1 for t in tasks if not t["gold"]),
           "allocation": {r: alloc[r] for r in sorted(alloc)}, "population_by_repo": dict(sorted(counts.items())),
           "ids": sorted(ids)}
    with open(a.out, "w") as f:
        json.dump(out, f, indent=1)
        f.write("\n")
    print(json.dumps({k: out[k] for k in ("seed", "n", "population", "excluded_no_gold", "allocation")}))


if __name__ == "__main__":
    main()
