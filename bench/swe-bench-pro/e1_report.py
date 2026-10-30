#!/usr/bin/env python3
"""E1's tables: gold-file recall of the pack, BM25 and truncation on SWE-bench Pro, with bootstrap intervals.

usage: e1_report.py --work WORK [--ids FILE] [--json OUT.json] [--md OUT.md]

Reads WORK/out/e1/rows/*.jsonl and WORK/out/e1/tasks/*.json (--ids limits it to a list of task ids, such as
sample-e2.json). Per budget and arm: all-gold, any-gold and the mean share of gold files inside, pooled over
tasks and per repository, with and without the tasks whose statement names a gold path; 95% bootstrap
percentile intervals over tasks (2,000 resamples, seed 1); the paired differences pack minus BM25 and pack
minus truncation with paired intervals; McNemar's exact test on all-gold, pack against BM25; the share of
tasks whose whole file set fits the budget; and the index time per repository.
"""
import argparse
import glob
import json
import math
import os
import random
import statistics

BUDGETS = ["16K", "32K", "64K", "128K", "256K", "1M"]
ARMS = ["pack", "bm25", "trunc"]
METRICS = ["all_gold", "any_gold", "share"]
NBOOT = 2000


def mean(xs):
    return sum(xs) / len(xs) if xs else None


def boot_ci(xs, rng, n=NBOOT):
    if not xs:
        return None
    ms = sorted(mean([xs[rng.randrange(len(xs))] for _ in xs]) for _ in range(n))
    return [ms[int(0.025 * n)], ms[min(n - 1, int(0.975 * n))]]


def mcnemar_exact(b, c):
    n = b + c
    if n == 0:
        return 1.0
    return min(1.0, 2 * sum(math.comb(n, i) for i in range(0, min(b, c) + 1)) / 2 ** n)


def load(work, ids=None, run="e1"):
    rows = {}
    for p in sorted(glob.glob(os.path.join(work, "out", run, "rows", "*.jsonl"))):
        tid = os.path.basename(p)[:-len(".jsonl")]
        if ids is not None and tid not in ids:
            continue
        for r in map(json.loads, open(p)):
            rows[(tid, r["arm"], r["budget"])] = r
    tasks = {}
    for p in glob.glob(os.path.join(work, "out", run, "tasks", "*.json")):
        tid = os.path.basename(p)[:-len(".json")]
        if ids is None or tid in ids:
            tasks[tid] = json.load(open(p))
    failed = [os.path.basename(p)[:-len(".json.failed")]
              for p in glob.glob(os.path.join(work, "out", run, "tasks", "*.json.failed"))
              if (ids is None or os.path.basename(p)[:-len(".json.failed")] in ids)
              and not os.path.exists(p[:-len(".failed")])]
    return rows, tasks, failed


def cell(rows, tids, budget, rng):
    """The statistics of one budget over the tasks tids (paired: every arm's row exists for each task)."""
    have = [t for t in tids if all((t, a, budget) in rows for a in ARMS)]
    out = {"n": len(have)}
    if not have:
        return out
    out["fits_whole"] = mean([1.0 if rows[(t, "trunc", budget)]["fits_whole"] else 0.0 for t in have])
    for a in ARMS:
        for m in METRICS:
            xs = [float(rows[(t, a, budget)][m]) for t in have]
            out[f"{a}_{m}"] = mean(xs)
            out[f"{a}_{m}_ci"] = boot_ci(xs, rng)
        out[f"{a}_tokens"] = mean([rows[(t, a, budget)]["context_tokens"] or 0 for t in have])
    out["pack_whole_share"] = mean([rows[(t, "pack", budget)].get("inside_whole", 0) / rows[(t, "pack", budget)]["gold_n"]
                                    for t in have])
    for other in ("bm25", "trunc"):
        for m in METRICS:
            d = [float(rows[(t, "pack", budget)][m]) - float(rows[(t, other, budget)][m]) for t in have]
            out[f"pack_minus_{other}_{m}"] = mean(d)
            out[f"pack_minus_{other}_{m}_ci"] = boot_ci(d, rng)
    b = sum(1 for t in have if rows[(t, "pack", budget)]["all_gold"] and not rows[(t, "bm25", budget)]["all_gold"])
    c = sum(1 for t in have if rows[(t, "bm25", budget)]["all_gold"] and not rows[(t, "pack", budget)]["all_gold"])
    out["all_gold_pack_only"], out["all_gold_bm25_only"], out["all_gold_mcnemar_p"] = b, c, mcnemar_exact(b, c)
    return out


def pct(x):
    return "" if x is None else f"{100 * x:.1f}"


def ci(x):
    return "" if not x else f"{100 * x[0]:.1f}-{100 * x[1]:.1f}"


def table(scope_rows, title):
    lines = [f"### {title}", "",
             "| Budget | Tasks | Fits whole | pack all-gold | BM25 all-gold | truncation all-gold | pack - BM25 | "
             "pack any-gold | BM25 any-gold | truncation any-gold |",
             "|---|---|---|---|---|---|---|---|---|---|"]
    for bud in BUDGETS:
        c = scope_rows.get(bud) or {}
        if not c.get("n"):
            continue
        lines.append(
            f"| {bud} | {c['n']} | {pct(c['fits_whole'])}% | {pct(c['pack_all_gold'])} ({ci(c['pack_all_gold_ci'])}) | "
            f"{pct(c['bm25_all_gold'])} ({ci(c['bm25_all_gold_ci'])}) | {pct(c['trunc_all_gold'])} | "
            f"{pct(c['pack_minus_bm25_all_gold'])} ({ci(c['pack_minus_bm25_all_gold_ci'])}) | "
            f"{pct(c['pack_any_gold'])} ({ci(c['pack_any_gold_ci'])}) | {pct(c['bm25_any_gold'])} "
            f"({ci(c['bm25_any_gold_ci'])}) | {pct(c['trunc_any_gold'])} |")
    return lines


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True)
    ap.add_argument("--ids")
    ap.add_argument("--run", default="e1", help="the E1 run: e1.py's --run name (a folder under WORK/out)")
    ap.add_argument("--json")
    ap.add_argument("--md")
    a = ap.parse_args()
    ids = None
    if a.ids:
        d = json.load(open(a.ids))
        ids = set(d["ids"] if isinstance(d, dict) else d)
    rows, tasks, failed = load(a.work, ids, a.run)
    tids = sorted({t for t, _a, _b in rows})
    repo_of = {t: tasks[t]["repo"] for t in tids if t in tasks}
    named = {t for (t, _a, _b), r in rows.items() if r["names_gold_path"]}
    rng = random.Random(1)
    scopes = {"all": tids, "without_named": [t for t in tids if t not in named]}
    for r in sorted(set(repo_of.values())):
        scopes["repo:" + r] = [t for t in tids if repo_of.get(t) == r]
    out = {"tasks": len(tids), "failed": sorted(failed), "named": len(named), "scopes": {}}
    for s, ts in scopes.items():
        out["scopes"][s] = {b: cell(rows, ts, b, rng) for b in BUDGETS}
    index = {}
    for t, info in tasks.items():
        ix = info.get("index") or {}
        index.setdefault(info["repo"], []).append((ix.get("wall_seconds"), ix.get("cpu_seconds"), ix.get("data_bytes"),
                                                   info.get("tokens_eligible"), info.get("seconds")))
    out["index"] = {r: {"tasks": len(v), "index_wall_median_s": statistics.median(x[0] for x in v),
                        "index_wall_max_s": max(x[0] for x in v), "index_cpu_median_s": statistics.median(x[1] for x in v),
                        "index_mib_median": round(statistics.median(x[2] for x in v) / 2 ** 20),
                        "tokens_eligible_median": statistics.median(x[3] for x in v),
                        "task_seconds_median": statistics.median(x[4] for x in v)} for r, v in sorted(index.items())}
    if a.json:
        with open(a.json, "w") as f:
            json.dump(out, f, indent=1, sort_keys=True)
    md = [f"Tasks measured: {len(tids)} (failed: {len(failed)}); statements naming a gold path: {len(named)}.", ""]
    md += table(out["scopes"]["all"], "All measured tasks: % of tasks (95% bootstrap interval)") + [""]
    md += table(out["scopes"]["without_named"], "Without the tasks whose statement names a gold path") + [""]
    for bud in ("64K", "256K"):
        md += [f"### Per repository at {bud}: all-gold % (95% bootstrap interval)", "",
               "| Repository | Tasks | pack | BM25 | truncation | pack - BM25 |", "|---|---|---|---|---|---|"]
        for s in sorted(k for k in out["scopes"] if k.startswith("repo:")):
            c = out["scopes"][s].get(bud) or {}
            if c.get("n"):
                md.append(f"| {s[5:]} | {c['n']} | {pct(c['pack_all_gold'])} ({ci(c['pack_all_gold_ci'])}) | "
                          f"{pct(c['bm25_all_gold'])} ({ci(c['bm25_all_gold_ci'])}) | {pct(c['trunc_all_gold'])} | "
                          f"{pct(c['pack_minus_bm25_all_gold'])} ({ci(c['pack_minus_bm25_all_gold_ci'])}) |")
        md.append("")
    md += ["### Index time and size per repository", "",
           "| Repository | Tasks | Index s (median) | Index s (max) | Index CPU s (median) | Index MiB (median) | "
           "Tokens in the file set (median) | Task s (median) |", "|---|---|---|---|---|---|---|---|"]
    for r, v in out["index"].items():
        md.append(f"| {r} | {v['tasks']} | {v['index_wall_median_s']:.0f} | {v['index_wall_max_s']:.0f} | "
                  f"{v['index_cpu_median_s']:.0f} | {v['index_mib_median']} | {v['tokens_eligible_median']:,.0f} | "
                  f"{v['task_seconds_median']:.0f} |")
    text = "\n".join(md) + "\n"
    if a.md:
        with open(a.md, "w") as f:
            f.write(text)
    print(text)


if __name__ == "__main__":
    main()
