#!/usr/bin/env python3
"""E1 for two builds side by side: coverage, the rows both measured, and all-gold recall per budget.

usage: e1_compare.py --work WORK [--a e1] --b RUN [--ids FILE] [--json OUT.json] [--md OUT.md]

Runs are e1.py's run names (folders under WORK/out): e1 by default, and a later build's --run name, such as
e1-0.1.10.

- Coverage: the tasks each run measured and the tasks each left out (failed twice).
- Identity: for the tasks both measured, each (task, arm, budget) row compared on inside, all_gold, any_gold,
  context_tokens and context_files. BM25 and truncation do not depend on the Graphicle build, so they must
  match; the pack's rows show what the build changed.
- All-gold recall, % of tasks, per arm and budget: run A on its tasks, run B on its tasks, and run B on the
  tasks run A left out.
"""
import argparse
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from e1_report import ARMS, BUDGETS, load, mean  # noqa: E402

KEYS = ("inside", "all_gold", "any_gold", "context_tokens", "context_files")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True)
    ap.add_argument("--a", default="e1", help="the first E1 run (e1.py --run)")
    ap.add_argument("--b", required=True, help="the other E1 run, such as a later build's --run name")
    ap.add_argument("--ids")
    ap.add_argument("--json")
    ap.add_argument("--md")
    a = ap.parse_args()
    ids = None
    if a.ids:
        d = json.load(open(a.ids))
        ids = set(d["ids"] if isinstance(d, dict) else d)
    ra, _ta, fa = load(a.work, ids, a.a)
    rb, _tb, fb = load(a.work, ids, a.b)
    tasks_a = {t for t, _x, _y in ra}
    tasks_b = {t for t, _x, _y in rb}
    common = sorted(tasks_a & tasks_b)
    only_b = sorted(tasks_b - tasks_a)
    out = {"a": a.a, "b": a.b, "measured_a": len(tasks_a), "measured_b": len(tasks_b), "failed_a": sorted(fa),
           "failed_b": sorted(fb), "common": len(common), "only_b": only_b, "identity": {}, "all_gold": {}}
    for arm in ARMS:
        same = diff = 0
        for k in ra:
            if k[0] in tasks_b and k[1] == arm and k in rb:
                if all(ra[k].get(x) == rb[k].get(x) for x in KEYS):
                    same += 1
                else:
                    diff += 1
        out["identity"][arm] = {"same": same, "different": diff}
    for bud in BUDGETS:
        row = {}
        for arm in ARMS:
            for name, rows, ts in (("a", ra, sorted(tasks_a)), ("b", rb, sorted(tasks_b)), ("b_only", rb, only_b)):
                xs = [rows[(t, arm, bud)]["all_gold"] for t in ts if (t, arm, bud) in rows]
                row[f"{name}_{arm}"] = (mean(xs), len(xs))
        out["all_gold"][bud] = row
    md = [f"E1, build A ({a.a}) beside build B ({a.b}).", "",
          f"- Measured: A {len(tasks_a)} tasks (left out {len(fa)}), B {len(tasks_b)} (left out {len(fb)}); "
          f"both {len(common)}; only B {len(only_b)}.",
          "- Rows on the tasks both measured, the same in both builds: " + "; ".join(
              f"{arm} {v['same']} of {v['same'] + v['different']}" for arm, v in out["identity"].items()) + ".", "",
          "| Budget | A pack | A BM25 | B pack | B BM25 | B pack, tasks only B measured | B BM25, same tasks |",
          "|---|---|---|---|---|---|---|"]

    def f(v):
        return "" if v[0] is None else f"{100 * v[0]:.1f} (n={v[1]})"
    for bud, r in out["all_gold"].items():
        if r["a_pack"][1] or r["b_pack"][1]:
            md.append(f"| {bud} | {f(r['a_pack'])} | {f(r['a_bm25'])} | {f(r['b_pack'])} | {f(r['b_bm25'])} | "
                      f"{f(r['b_only_pack'])} | {f(r['b_only_bm25'])} |")
    text = "\n".join(md) + "\n"
    if a.json:
        with open(a.json, "w") as fh:
            json.dump(out, fh, indent=1, sort_keys=True)
    if a.md:
        with open(a.md, "w") as fh:
            fh.write(text)
    print(text)


if __name__ == "__main__":
    main()
