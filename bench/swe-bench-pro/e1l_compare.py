#!/usr/bin/env python3
"""Two E1L runs side by side (a pack build against another, the same tasks): E1's file-level readings and E1L's
line-level metrics per budget, each build's value, the change paired by task with its 95% interval, and each
build's gap against BM25.

usage: e1l_compare.py --work WORK --base RUN --new RUN [--e1-run RUN] [--json OUT] [--md OUT]

Over the tasks both runs measured (and, for --e1-run, E1's own rows of the base build as a check):
  E1 registered: every gold file inside, a gold file inside when the pack holds some of its text (`included`);
  E1 text only: every gold file inside with text (a whole file or an excerpt; an outline alone does not count);
  E1 whole files: every gold file held whole (BM25 and truncation hold whole files only);
  (a) every gold line inside; (b) the mean share of gold lines inside; (c) the gold files with all their gold
  lines inside, pooled (tasks with gold lines).
Intervals: 95% bootstrap percentiles, 2,000 resamples of tasks, seed 1 per cell, paired by task.
"""
import argparse
import glob
import json
import os
import random

BUDGETS = ["16K", "32K", "64K", "128K", "256K", "1M"]
NBOOT = 2000


def load(work, run):
    rows = {}
    for p in sorted(glob.glob(os.path.join(work, "out", run, "rows", "*.jsonl"))):
        for r in map(json.loads, open(p)):
            rows[(r["id"], r["arm"], r["budget"])] = r
    return rows


def mean(xs):
    return sum(xs) / len(xs) if xs else None


def boot(xs, rng):
    if not xs:
        return None
    n, vals = len(xs), []
    for _ in range(NBOOT):
        vals.append(sum(xs[rng.randrange(n)] for _ in range(n)) / n)
    vals.sort()
    return [vals[int(0.025 * NBOOT)], vals[min(NBOOT - 1, int(0.975 * NBOOT))]]


def ratio_boot(pairs, rng):
    if not pairs:
        return None
    n, vals = len(pairs), []
    for _ in range(NBOOT):
        s = [pairs[rng.randrange(n)] for _ in range(n)]
        d = sum(b for _a, b in s)
        vals.append(sum(a for a, _b in s) / d if d else 0.0)
    vals.sort()
    return [vals[int(0.025 * NBOOT)], vals[min(NBOOT - 1, int(0.975 * NBOOT))]]


def readings(r):
    """E1's three file-level readings and E1L's metrics of one pack (or BM25) row."""
    files = r["files"]
    with_lines = [f for f in files if f["n_lines"]]
    return {"registered": float(r["file_inside"] == r["gold_n"]),
            "text_only": float(all(f["in_context"] and f.get("text", f.get("whole")) for f in files)),
            "whole": float(all(f.get("whole") for f in files)),
            "a": None if r["all_lines"] is None else float(r["all_lines"]),
            "b": r["line_share"],
            "c": (r["files_all_lines"], r["files_with_lines"]) if with_lines else None}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True)
    ap.add_argument("--base", required=True, help="the earlier E1L run (a folder under WORK/out), such as e1l-0.1.9")
    ap.add_argument("--new", required=True)
    ap.add_argument("--e1-run", help="E1's run of the base build (e1.py rows), for its registered all-gold")
    ap.add_argument("--json")
    ap.add_argument("--md")
    a = ap.parse_args()
    base, new = load(a.work, a.base), load(a.work, a.new)
    e1 = load(a.work, a.e1_run) if a.e1_run else {}
    tids_b = {t for t, _a, _b in base}
    tids_n = {t for t, _a, _b in new}
    both = sorted(tids_b & tids_n)
    out = {"base": a.base, "new": a.new, "tasks_base": len(tids_b), "tasks_new": len(tids_n), "tasks_both": len(both),
           "only_base": len(tids_b - tids_n), "only_new": len(tids_n - tids_b), "budgets": {}}
    for bud in BUDGETS:
        ts = [t for t in both if (t, "pack", bud) in base and (t, "pack", bud) in new and (t, "bm25", bud) in new]
        if not ts:
            continue
        cell = {"n": len(ts)}
        rb = {t: readings(base[(t, "pack", bud)]) for t in ts}
        rn = {t: readings(new[(t, "pack", bud)]) for t in ts}
        bm = {t: readings(new[(t, "bm25", bud)]) for t in ts}
        for key in ("registered", "text_only", "whole", "a", "b"):
            tk = [t for t in ts if rb[t][key] is not None and rn[t][key] is not None]
            xb, xn, xm = [rb[t][key] for t in tk], [rn[t][key] for t in tk], [bm[t][key] for t in tk]
            rng = random.Random(1)
            cell[key] = {"n": len(tk), "base": mean(xb), "base_ci": boot(xb, rng), "new": mean(xn),
                         "new_ci": boot(xn, rng), "bm25": mean(xm),
                         "change": mean([y - x for x, y in zip(xb, xn)]),
                         "change_ci": boot([y - x for x, y in zip(xb, xn)], rng),
                         "gap_base": mean([x - z for x, z in zip(xb, xm)]),
                         "gap_base_ci": boot([x - z for x, z in zip(xb, xm)], rng),
                         "gap_new": mean([y - z for y, z in zip(xn, xm)]),
                         "gap_new_ci": boot([y - z for y, z in zip(xn, xm)], rng)}
        tk = [t for t in ts if rb[t]["c"] and rn[t]["c"]]
        pb, pn, pm = [rb[t]["c"] for t in tk], [rn[t]["c"] for t in tk], [bm[t]["c"] for t in tk]
        rng = random.Random(1)
        tot = sum(d for _x, d in pb)
        cell["c"] = {"n": len(tk), "base": sum(x for x, _d in pb) / tot if tot else None, "base_ci": ratio_boot(pb, rng),
                     "new": sum(x for x, _d in pn) / tot if tot else None, "new_ci": ratio_boot(pn, rng),
                     "bm25": sum(x for x, _d in pm) / tot if tot else None,
                     "change": (sum(x for x, _d in pn) - sum(x for x, _d in pb)) / tot if tot else None,
                     "change_ci": ratio_boot([(y[0] - x[0], x[1]) for x, y in zip(pb, pn)], rng)}
        if e1:
            xs = [float(e1[(t, "pack", bud)]["inside"] == e1[(t, "pack", bud)]["gold_n"]) for t in ts
                  if (t, "pack", bud) in e1]
            cell["e1_base_registered"] = {"n": len(xs), "value": mean(xs)}
        out["budgets"][bud] = cell
    if a.json:
        json.dump(out, open(a.json, "w"), indent=1, sort_keys=True)

    def pc(x):
        return "-" if x is None else f"{100 * x:.1f}"

    def sg(x):  # a change or a gap, in points, with its sign
        return "-" if x is None else f"{100 * x:+.1f}"

    def ci(x):
        return "" if not x else f" ({100 * x[0]:+.1f} to {100 * x[1]:+.1f})"
    md = [f"Tasks: {a.base} {out['tasks_base']}, {a.new} {out['tasks_new']}, both {out['tasks_both']} "
          f"(only {a.base}: {out['only_base']}, only {a.new}: {out['only_new']}).", ""]
    titles = [("registered", "E1 registered: every gold file inside (any of its text), %"),
              ("text_only", "E1 text only: every gold file inside as text (an outline alone does not count), %"),
              ("whole", "E1 whole files: every gold file held whole, %"),
              ("a", "E1L (a): every gold line inside, %"),
              ("b", "E1L (b): mean share of gold lines inside, %"),
              ("c", "E1L (c): gold files with all their gold lines inside, % (pooled)")]
    for key, title in titles:
        # (c) is pooled over tasks, so it has no paired gap against BM25: its table leaves those columns out.
        gaps = key != "c"
        head = f"| Budget | Tasks | {a.base} | {a.new} | change (paired 95% interval) | BM25 |"
        md += [f"### {title}", "", head + (f" {a.base} - BM25 | {a.new} - BM25 |" if gaps else ""),
               "|---|---|---|---|---|---|" + ("---|---|" if gaps else "")]
        for bud in BUDGETS:
            c = out["budgets"].get(bud, {}).get(key)
            if not c:
                continue
            row = (f"| {bud} | {c['n']} | {pc(c['base'])} | {pc(c['new'])} | {sg(c['change'])}{ci(c['change_ci'])} | "
                   f"{pc(c['bm25'])} |")
            if gaps:
                row += (f" {sg(c['gap_base'])}{ci(c['gap_base_ci'])} | {sg(c['gap_new'])}{ci(c['gap_new_ci'])} |")
            md.append(row)
        md.append("")
    if e1:
        md += [f"E1's own rows of the base build ({a.e1_run}), registered all-gold on the same tasks: " + "; ".join(
            f"{bud} {pc(out['budgets'][bud]['e1_base_registered']['value'])}" for bud in BUDGETS
            if bud in out["budgets"] and "e1_base_registered" in out["budgets"][bud]) + "."]
    text = "\n".join(md) + "\n"
    if a.md:
        open(a.md, "w").write(text)
    print(text)


if __name__ == "__main__":
    main()
