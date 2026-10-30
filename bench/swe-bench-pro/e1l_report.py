#!/usr/bin/env python3
"""E1L's tables: line-level recall of the pack, BM25 and truncation on SWE-bench Pro, and why the pack misses.

usage: e1l_report.py --work WORK [--run e1l] [--e1-run e1] [--json OUT.json] [--md OUT.md]

Metrics per arm and budget, over the tasks with gold lines (95% bootstrap percentile intervals over tasks, 2,000
resamples, seed 1 per cell; gaps are pack minus BM25, paired by task):
  (a) all lines: the share of tasks with every gold line inside;
  (b) line share: the mean over tasks of the share of gold lines inside;
  (c) files: the share of gold files (with gold lines) whose gold lines are all inside, pooled over tasks (the
      interval resamples tasks).
Diagnosis at 16K, 64K and 256K: each gold file's state in the pack (absent, outline only, excerpt with none of its
gold lines, some, all, whole), file sizes against the pack's per-file caps, the ranks of absent files, and the
tasks where BM25 holds every gold line and the pack does not (by ranking: a gold file absent from the pack; by
holding: every gold file in the pack, some without all their lines). Also: E1's file-level rows against the
run named by --e1-run.
"""
import argparse
import glob
import json
import os
import random
import statistics
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from e1_report import boot_ci, mean  # noqa: E402

BUDGETS = ["16K", "32K", "64K", "128K", "256K", "1M"]
TOKENS = {"16K": 16384, "32K": 32768, "64K": 65536, "128K": 131072, "256K": 262144, "1M": 1015808}
ARMS = ["pack", "bm25", "trunc"]
DIAG = ["16K", "64K", "256K"]
STATES = ["absent", "outline only", "excerpt, no gold lines", "excerpt, some gold lines", "excerpt, all gold lines",
          "whole"]
NBOOT = 2000


def caps(label, rank):
    """The pack's per-file caps at a budget (code_pack.rs Sizing, 0.1.9): (focus whole cap, per-file cap)."""
    b = TOKENS[label]
    small = b <= 256_000
    per_file = max(b // (10 if small else 8), 2_000)
    whole_limit = min(max(b // 16, 1_500), 16_000)
    cap = 1_500 if (small and rank is not None and rank > 15) else whole_limit
    return min(cap, per_file), per_file


def state(f):
    if not f["in_context"]:
        return "absent"
    if f.get("whole"):
        return "whole"
    if not f.get("text"):
        return "outline only" if f.get("outline") else "absent"
    if f["inside_lines"] == 0:
        return "excerpt, no gold lines"
    if f["inside_lines"] < f["n_lines"]:
        return "excerpt, some gold lines"
    return "excerpt, all gold lines"


def ratio_ci(pairs, rng):
    """95% interval of sum(x)/sum(n) over tasks, resampling tasks."""
    if not pairs:
        return None
    vals = []
    for _ in range(NBOOT):
        s = [pairs[rng.randrange(len(pairs))] for _ in pairs]
        n = sum(b for _a, b in s)
        vals.append(sum(a for a, _b in s) / n if n else 0.0)
    vals.sort()
    return [vals[int(0.025 * NBOOT)], vals[min(NBOOT - 1, int(0.975 * NBOOT))]]


def ratio_gap_ci(trip, rng):
    """95% interval of the paired gap of two pooled shares (x1/n - x2/n) over tasks."""
    vals = []
    for _ in range(NBOOT):
        s = [trip[rng.randrange(len(trip))] for _ in trip]
        n = sum(c for _a, _b, c in s)
        vals.append((sum(a for a, _b, _c in s) - sum(b for _a, b, _c in s)) / n if n else 0.0)
    vals.sort()
    return [vals[int(0.025 * NBOOT)], vals[min(NBOOT - 1, int(0.975 * NBOOT))]]


def cell(rows, tids, bud):
    have = [t for t in tids if all((t, a, bud) in rows for a in ARMS) and rows[(t, "pack", bud)]["gold_lines"]]
    out = {"n": len(have)}
    if not have:
        return out
    for arm in ARMS:
        rng = random.Random(1)
        xa = [float(rows[(t, arm, bud)]["all_lines"]) for t in have]
        xb = [rows[(t, arm, bud)]["line_share"] for t in have]
        pc = [(rows[(t, arm, bud)]["files_all_lines"], rows[(t, arm, bud)]["files_with_lines"]) for t in have]
        xf = [float(rows[(t, arm, bud)]["file_inside"] == rows[(t, arm, bud)]["gold_n"]) for t in have]
        out[arm] = {"all_lines": mean(xa), "all_lines_ci": boot_ci(xa, rng), "line_share": mean(xb),
                    "line_share_ci": boot_ci(xb, rng), "files": sum(a for a, _ in pc) / sum(b for _, b in pc),
                    "files_ci": ratio_ci(pc, rng), "all_files": mean(xf)}
    for other in ("bm25", "trunc"):
        rng = random.Random(1)
        da = [float(rows[(t, "pack", bud)]["all_lines"]) - float(rows[(t, other, bud)]["all_lines"]) for t in have]
        db = [rows[(t, "pack", bud)]["line_share"] - rows[(t, other, bud)]["line_share"] for t in have]
        tr = [(rows[(t, "pack", bud)]["files_all_lines"], rows[(t, other, bud)]["files_all_lines"],
               rows[(t, "pack", bud)]["files_with_lines"]) for t in have]
        out[f"gap_{other}"] = {"all_lines": mean(da), "all_lines_ci": boot_ci(da, rng), "line_share": mean(db),
                               "line_share_ci": boot_ci(db, rng),
                               "files": out["pack"]["files"] - out[other]["files"],
                               "files_ci": ratio_gap_ci(tr, rng)}
    return out


def pc(x):
    return "" if x is None else f"{100 * x:.1f}"


def ci(x):
    return "" if not x else f"{100 * x[0]:.1f} to {100 * x[1]:.1f}"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True)
    ap.add_argument("--run", default="e1l")
    ap.add_argument("--e1-run", default="e1",
                    help="E1's run of the same build (e1.py rows), checked against these rows")
    ap.add_argument("--json")
    ap.add_argument("--md")
    a = ap.parse_args()
    rows, tasks = {}, {}
    for p in sorted(glob.glob(os.path.join(a.work, "out", a.run, "rows", "*.jsonl"))):
        for r in map(json.loads, open(p)):
            rows[(r["id"], r["arm"], r["budget"])] = r
    for p in glob.glob(os.path.join(a.work, "out", a.run, "tasks", "*.json")):
        info = json.load(open(p))
        tasks[info["id"]] = info
    failed = sorted(os.path.basename(p)[:-len(".json.failed")]
                    for p in glob.glob(os.path.join(a.work, "out", a.run, "tasks", "*.json.failed"))
                    if not os.path.exists(p[:-len(".failed")]))
    tids = sorted({t for t, _a, _b in rows})
    repos = sorted({tasks[t]["repo"] for t in tids})
    out = {"tasks": len(tids), "failed": failed, "scopes": {}}
    # The gold lines and their check against the files at the base commit.
    gold = {r["id"]: r for r in map(json.loads, open(os.path.join(a.work, "e1l", "gold.jsonl")))}
    nfiles = sum(len(gold[t]["files"]) for t in tids)
    nlines = sum(len(f["lines"]) for t in tids for f in gold[t]["files"].values())
    nolines = sum(1 for t in tids for f in gold[t]["files"].values() if not f["lines"])
    out["gold"] = {"files": nfiles, "lines": nlines, "files_without_lines": nolines,
                   "minus_checked": sum(tasks[t]["check"]["minus_checked"] for t in tids),
                   "minus_mismatch": sum(len(tasks[t]["check"]["minus_mismatch"]) for t in tids),
                   "point_out_of_range": sum(len(tasks[t]["check"]["point_out_of_range"]) for t in tids),
                   "tasks_without_lines": sum(1 for t in tids if not any(f["lines"] for f in gold[t]["files"].values()))}
    scopes = {"all": tids, "without_named": [t for t in tids if not rows[(t, "pack", "16K")]["names_gold_path"]]}
    for r in repos:
        scopes["repo:" + r] = [t for t in tids if tasks[t]["repo"] == r]
    for s, ts in scopes.items():
        out["scopes"][s] = {b: cell(rows, ts, b) for b in BUDGETS}
    # Diagnosis.
    diag = {}
    for bud in DIAG:
        files = []
        for t in tids:
            if (t, "pack", bud) not in rows:
                continue
            bm = {f["path"]: f for f in rows[(t, "bm25", bud)]["files"]}
            for f in rows[(t, "pack", bud)]["files"]:
                if not f["n_lines"]:
                    continue
                size = tasks[t]["sizes"][f["path"]]
                whole_cap, per_file = caps(bud, f.get("rank"))
                files.append({"id": t, "repo": tasks[t]["repo"], "path": f["path"], "state": state(f),
                              "tokens": size["tokens"], "lines": size["lines"], "minified": size["minified"],
                              "rank": f.get("rank"), "omitted_listed": f.get("omitted_listed"),
                              "n_lines": f["n_lines"], "inside_lines": f["inside_lines"],
                              "outline_lines": f.get("outline_lines", 0), "min_dist": f.get("min_dist"),
                              "over_whole_cap": size["tokens"] is not None and size["tokens"] > whole_cap,
                              "over_per_file": size["tokens"] is not None and size["tokens"] > per_file,
                              "bm25_in": bm[f["path"]]["in_context"], "bm25_rank": bm[f["path"]].get("rank"),
                              "ranges": f.get("ranges", [])[:6]})
        d = {"files": len(files), "states": {}}
        for st in STATES:
            fs = [f for f in files if f["state"] == st]
            toks = [f["tokens"] for f in fs if f["tokens"] is not None and f["tokens"] >= 0]
            d["states"][st] = {
                "files": len(fs), "share": len(fs) / len(files) if files else None,
                "tokens_median": statistics.median(toks) if toks else None,
                "over_whole_cap": sum(1 for f in fs if f["over_whole_cap"]),
                "over_per_file": sum(1 for f in fs if f["over_per_file"]),
                "minified": sum(1 for f in fs if f["minified"]),
                "bm25_holds": sum(1 for f in fs if f["bm25_in"]),
                "outline_lines": sum(f["outline_lines"] for f in fs),
                "rank_median": statistics.median([f["rank"] for f in fs if f["rank"] is not None])
                if any(f["rank"] is not None for f in fs) else None,
                "rank_listed": sum(1 for f in fs if f["rank"] is not None),
                "rank_le_15": sum(1 for f in fs if f["rank"] is not None and f["rank"] <= 15),
                "min_dist_median": statistics.median([f["min_dist"] for f in fs if f["min_dist"] is not None])
                if any(f["min_dist"] is not None for f in fs) else None}
        # BM25 holds every gold line, the pack does not: by ranking or by holding?
        wins = {"bm25_only": 0, "pack_only": 0, "by_ranking": 0, "by_holding": 0}
        for t in tids:
            p, b = rows.get((t, "pack", bud)), rows.get((t, "bm25", bud))
            if not p or not p["gold_lines"]:
                continue
            if b["all_lines"] and not p["all_lines"]:
                wins["bm25_only"] += 1
                absent = any(not f["in_context"] for f in p["files"] if f["n_lines"])
                wins["by_ranking" if absent else "by_holding"] += 1
            elif p["all_lines"] and not b["all_lines"]:
                wins["pack_only"] += 1
        d["bm25_wins"] = wins
        rng = random.Random(20261028)
        d["examples"] = {}
        for st in STATES[:4]:
            fs = sorted((f for f in files if f["state"] == st), key=lambda f: (f["id"], f["path"]))
            d["examples"][st] = rng.sample(fs, min(3, len(fs)))
        diag[bud] = d
    out["diagnosis"] = diag
    # E1's file-level rows against the E1 run.
    ident = {}
    for arm in ARMS:
        same = diff = 0
        for t in tids:
            for bud in BUDGETS:
                r, p = rows.get((t, arm, bud)), os.path.join(a.work, "out", a.e1_run, "rows", t + ".jsonl")
                if r is None or not os.path.exists(p):
                    continue
                e = {(x["arm"], x["budget"]): x for x in map(json.loads, open(p))}.get((arm, bud))
                if e is None:
                    continue
                ok = (e["inside"] == r["file_inside"] and e["context_tokens"] == r["context_tokens"]
                      and e["context_files"] == r["context_files"])
                same, diff = same + ok, diff + (not ok)
        ident[arm] = [same, same + diff]
    out["identity_vs_e1"] = ident
    if a.json:
        with open(a.json, "w") as f:
            json.dump(out, f, indent=1, sort_keys=True)
    # Markdown.
    g = out["gold"]
    md = [f"Tasks measured: {len(tids)} (failed: {len(failed)}). Gold files {g['files']}, gold lines {g['lines']}, "
          f"files without gold lines {g['files_without_lines']}, tasks without gold lines {g['tasks_without_lines']}. "
          f"'-' lines checked against the base commit: {g['minus_checked']}, mismatched {g['minus_mismatch']}; "
          f"insertion points out of range {g['point_out_of_range']}.",
          "File-level rows against " + a.e1_run + " (inside, context tokens and files): " + "; ".join(
              f"{arm} {v[0]} of {v[1]}" for arm, v in ident.items()) + ".", ""]
    for key, title in (("all_lines", "(a) Tasks with every gold line inside, %"),
                       ("line_share", "(b) Mean share of gold lines inside, %"),
                       ("files", "(c) Gold files with all their gold lines inside, % (pooled)")):
        md += [f"### {title} (95% interval; gap = pack minus BM25, paired)", "",
               "| Budget | Tasks | pack | BM25 | truncation | pack - BM25 |", "|---|---|---|---|---|---|"]
        for bud in BUDGETS:
            c = out["scopes"]["all"][bud]
            if c.get("n"):
                md.append(f"| {bud} | {c['n']} | {pc(c['pack'][key])} ({ci(c['pack'][key + '_ci'])}) | "
                          f"{pc(c['bm25'][key])} ({ci(c['bm25'][key + '_ci'])}) | {pc(c['trunc'][key])} | "
                          f"{pc(c['gap_bm25'][key])} ({ci(c['gap_bm25'][key + '_ci'])}) |")
        md.append("")
    md += ["### File level from the same run (E1's rule for the pack: any text of the file)", "",
           "| Budget | pack all-gold | BM25 all-gold |", "|---|---|---|"]
    for bud in BUDGETS:
        c = out["scopes"]["all"][bud]
        if c.get("n"):
            md.append(f"| {bud} | {pc(c['pack']['all_files'])} | {pc(c['bm25']['all_files'])} |")
    for bud in ("64K", "256K"):
        md += ["", f"### Per repository at {bud}: (a) / (b) / (c), % (gap in (a), paired 95% interval)", "",
               "| Repository | Tasks | pack (a) | BM25 (a) | gap (a) | pack (b) | BM25 (b) | pack (c) | BM25 (c) |",
               "|---|---|---|---|---|---|---|---|---|"]
        for s in sorted(k for k in out["scopes"] if k.startswith("repo:")):
            c = out["scopes"][s].get(bud) or {}
            if c.get("n"):
                md.append(f"| {s[5:]} | {c['n']} | {pc(c['pack']['all_lines'])} | {pc(c['bm25']['all_lines'])} | "
                          f"{pc(c['gap_bm25']['all_lines'])} ({ci(c['gap_bm25']['all_lines_ci'])}) | "
                          f"{pc(c['pack']['line_share'])} | {pc(c['bm25']['line_share'])} | {pc(c['pack']['files'])} | "
                          f"{pc(c['bm25']['files'])} |")
    for bud, d in diag.items():
        md += ["", f"### Diagnosis at {bud}: the pack's state for each gold file with gold lines ({d['files']} files)",
               "", "| State | Files | % | Median tokens | Over whole cap | Over per-file cap | Minified | BM25 holds it | "
               "Rank listed (<= 15) | Median rank | Median distance to an excerpt (lines) |",
               "|---|---|---|---|---|---|---|---|---|---|---|"]
        for st, v in d["states"].items():
            md.append(f"| {st} | {v['files']} | {pc(v['share'])} | {v['tokens_median']} | {v['over_whole_cap']} | "
                      f"{v['over_per_file']} | {v['minified']} | {v['bm25_holds']} | {v['rank_listed']} "
                      f"({v['rank_le_15']}) | {v['rank_median']} | {v['min_dist_median']} |")
        w = d["bm25_wins"]
        md += ["", f"BM25 holds every gold line and the pack does not: {w['bm25_only']} tasks ({w['by_ranking']} by "
               f"ranking: a gold file absent from the pack; {w['by_holding']} by holding: every gold file in the pack, "
               f"some without all their lines). The pack holds every gold line and BM25 does not: {w['pack_only']}."]
    with open(a.md, "w") if a.md else open(os.devnull, "w") as f:
        f.write("\n".join(md) + "\n")
    print("\n".join(md))


if __name__ == "__main__":
    main()
