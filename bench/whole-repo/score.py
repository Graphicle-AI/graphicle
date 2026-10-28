#!/usr/bin/env python3
"""Scores for the whole-repository benchmark.

usage: score.py --runs DIR [DIR ...] --out FILE [--boot 2000] [--seed 1]

Each DIR is a run folder written by run.py (rows.jsonl and run.json). Writes a JSON summary with, per
run (arm and budget) and per repository plus pooled:
  n, hit@1, hit@3, hit@5 (a gold file among the first k answers), all-gold@5, MRR (reciprocal rank
  of the first gold file in the answer, 0 when none of the 10 is gold), context recall (pack and
  truncation: the share of gold files inside the context; the agent: the share of gold files it read),
  tokens in (cached and not), tokens out (reasoning included), cost, wall seconds and turns, with 95%
  bootstrap intervals (percentile, resampling questions) on the means;
  the same rows without the questions whose text names a gold file path;
  and paired comparisons between runs on the same questions: McNemar's exact test on hit@5.
"""
import argparse
import json
import math
import os
import random

METRICS = ["hit1", "hit3", "hit5", "allgold5", "rr", "context_recall", "tokens_in", "tokens_cached",
           "tokens_out", "tokens_reasoning", "cost_usd", "wall_seconds", "turns"]


def normalize_answer(files, file_set=None, limit=10):
    """Clean the model's paths: strip spaces, ./, a/ b/ prefixes, :line suffixes; resolve a leading
    folder that is not part of the repository when the rest is a file of it. Dedupe, keep order."""
    out = []
    for f in files or []:
        if not isinstance(f, str):
            continue
        p = f.strip().strip("`'\"")
        p = p.split("#L")[0]
        while p.startswith("./"):
            p = p[2:]
        if p[:2] in ("a/", "b/") and file_set is not None and p not in file_set and p[2:] in file_set:
            p = p[2:]
        p = p.lstrip("/")
        head, sep, tail = p.rpartition(":")
        if sep and tail.replace("-", "").isdigit():
            p = head
        if file_set is not None and p not in file_set and "/" in p and p.split("/", 1)[1] in file_set:
            p = p.split("/", 1)[1]
        if p and p not in out:
            out.append(p)
    return out[:limit]


def score_answer(files, gold):
    g = set(gold)
    first = next((i + 1 for i, f in enumerate(files) if f in g), None)
    return {
        "hit1": int(first is not None and first <= 1),
        "hit3": int(first is not None and first <= 3),
        "hit5": int(first is not None and first <= 5),
        "allgold5": int(g.issubset(set(files[:5]))),
        "rr": 1.0 / first if first else 0.0,
    }


def mean(xs):
    return sum(xs) / len(xs) if xs else None


def boot_ci(xs, rng, n):
    if not xs:
        return None
    ms = sorted(mean([xs[rng.randrange(len(xs))] for _ in xs]) for _ in range(n))
    return [ms[int(0.025 * n)], ms[min(n - 1, int(0.975 * n))]]


def mcnemar_exact(b, c):
    """Two-sided exact McNemar p-value from the discordant counts b and c."""
    n = b + c
    if n == 0:
        return 1.0
    k = min(b, c)
    p = sum(math.comb(n, i) for i in range(0, k + 1)) / 2 ** n
    return min(1.0, 2 * p)


def summarize(rows, rng, nboot):
    out = {"n": len(rows)}
    for m in METRICS:
        xs = [r[m] for r in rows if r.get(m) is not None]
        out[m] = mean(xs)
        if m in ("hit1", "hit3", "hit5", "allgold5", "rr", "context_recall", "cost_usd", "tokens_in", "tokens_out"):
            out[m + "_ci"] = boot_ci(xs, rng, nboot)
    out["errors"] = sum(1 for r in rows if r.get("error"))
    out["cost_usd_total"] = sum(r.get("cost_usd") or 0 for r in rows)
    return out


def load_run(d):
    meta = json.load(open(os.path.join(d, "run.json")))
    rows = [json.loads(l) for l in open(os.path.join(d, "rows.jsonl")) if l.strip()]
    return meta, rows


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--runs", nargs="+", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--boot", type=int, default=2000)
    ap.add_argument("--seed", type=int, default=1)
    a = ap.parse_args()
    runs = []
    seen = set()
    for d in a.runs:
        meta, rows = load_run(d)
        label = meta["label"]
        if label in seen:  # two runs of one arm (another api or model): name the second by its folder
            label = f"{label} [{os.path.basename(os.path.normpath(d))}]"
        seen.add(label)
        runs.append((label, meta, rows))
    result = {"runs": {}, "paired": []}
    for label, meta, rows in runs:
        rng = random.Random(a.seed)
        per = {}
        for repo in sorted({r["repo"] for r in rows}):
            rr = [r for r in rows if r["repo"] == repo]
            per[repo] = {"all": summarize(rr, rng, a.boot),
                         "without_named": summarize([r for r in rr if not r.get("names_gold_path")], rng, a.boot)}
        per["pooled"] = {"all": summarize(rows, rng, a.boot),
                         "without_named": summarize([r for r in rows if not r.get("names_gold_path")], rng, a.boot)}
        result["runs"][label] = {"meta": {k: meta.get(k) for k in
                                          ("arm", "budget", "budget_tokens", "model", "api", "counter", "context_source",
                                           "graphicle", "started", "finished", "spend_usd")}, "rows": per}
    for i in range(len(runs)):
        for j in range(i + 1, len(runs)):
            la, _, ra = runs[i]
            lb, _, rb = runs[j]
            ia = {r["id"]: r for r in ra if not r.get("error")}
            ib = {r["id"]: r for r in rb if not r.get("error")}
            common = sorted(set(ia) & set(ib))
            if not common:
                continue
            for scope in ["pooled"] + sorted({ia[k]["repo"] for k in common}):
                ks = [k for k in common if scope == "pooled" or ia[k]["repo"] == scope]
                b = sum(1 for k in ks if ia[k]["hit5"] and not ib[k]["hit5"])
                c = sum(1 for k in ks if ib[k]["hit5"] and not ia[k]["hit5"])
                result["paired"].append({"a": la, "b": lb, "scope": scope, "n": len(ks), "a_only": b, "b_only": c,
                                         "hit5_a": mean([ia[k]["hit5"] for k in ks]),
                                         "hit5_b": mean([ib[k]["hit5"] for k in ks]),
                                         "mcnemar_p": mcnemar_exact(b, c)})
    with open(a.out, "w") as f:
        json.dump(result, f, indent=1, sort_keys=True)
    for label in result["runs"]:
        p = result["runs"][label]["rows"]["pooled"]["all"]
        print(f"{label}: n={p['n']} hit@5={p['hit5']} MRR={p['rr']} cost=${p['cost_usd_total']:.2f} errors={p['errors']}")


if __name__ == "__main__":
    main()
