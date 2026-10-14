#!/usr/bin/env python3
"""Offline tuning of retrieve's file ranking from localize.py --features output.

Each features.jsonl row holds retrieve's candidate files with the parts of
their score (`whole`: whole-file BM25 / best file's; `defs`: the file's best
definitions; `test`: a test path) and the harness's BM25 file ranking. This
re-ranks the candidates with a formula and reports hit@k, so weights can be
chosen on one set of instances and then checked, unchanged, on another.

Usage: tune_ranking.py FEATURES.jsonl [--grid] [--formula NAME:ALPHA:TEST]
"""
import argparse, itertools, json, sys

KS = (1, 3, 5, 10)


def load(path):
    return [json.loads(line) for line in open(path) if line.strip()]


def hits(rows, rank_fn):
    out = {k: 0 for k in KS}
    mrr = 0.0
    for r in rows:
        files = rank_fn(r)
        gold = set(r["gold"])
        pos = next((i for i, f in enumerate(files) if f in gold), None)
        if pos is not None:
            mrr += 1 / (pos + 1)
            for k in KS:
                out[k] += pos < k
    n = max(len(rows), 1)
    res = {f"hit@{k}": round(v / n, 4) for k, v in out.items()}
    res["mrr"] = round(mrr / n, 4)
    return res


def current(r):
    return [c["file"] for c in r["ranking"]]


def bm25(r):
    return r["bm25"]


def blend(alpha, test_mult, rrf=False):
    """score = whole + alpha * defs / max(defs), times test_mult for tests."""
    def rank(r):
        cands = r["ranking"]
        if not cands:
            return []
        top_defs = max(c["defs"] for c in cands) or 1.0
        if rrf:
            by_whole = {c["file"]: i for i, c in enumerate(sorted(cands, key=lambda c: -c["whole"]))}
            by_defs = {c["file"]: i for i, c in enumerate(sorted(cands, key=lambda c: -c["defs"]))}
            def score(c):
                s = 1 / (60 + by_whole[c["file"]]) + alpha / (60 + by_defs[c["file"]])
                return s * (test_mult if c["test"] else 1.0)
        else:
            def score(c):
                s = c["whole"] + alpha * c["defs"] / top_defs
                return s * (test_mult if c["test"] else 1.0)
        return [c["file"] for c in sorted(cands, key=lambda c: (-score(c), c["file"]))]
    return rank


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("features", nargs="+")
    ap.add_argument("--grid", action="store_true")
    ap.add_argument("--formula", action="append", default=[], help="blend|rrf:ALPHA:TEST_MULT")
    args = ap.parse_args()
    rows = [r for p in args.features for r in load(p)]
    print(f"{len(rows)} instances")
    print("current retrieve ranking", hits(rows, current))
    print("harness BM25           ", hits(rows, bm25))
    for spec in args.formula:
        kind, a, t = spec.split(":")
        print(spec, hits(rows, blend(float(a), float(t), kind == "rrf")))
    if args.grid:
        results = []
        for kind, a, t in itertools.product(("blend", "rrf"), (0, 0.1, 0.2, 0.3, 0.5, 0.7, 1.0),
                                            (1.0, 0.8, 0.6, 0.5, 0.4, 0.3, 0.2)):
            h = hits(rows, blend(a, t, kind == "rrf"))
            results.append(((h["hit@1"] + h["hit@5"] + h["mrr"]) / 3, kind, a, t, h))
        results.sort(key=lambda x: -x[0])
        for obj, kind, a, t, h in results[:15]:
            print(f"{kind}:{a}:{t}", h)


if __name__ == "__main__":
    sys.exit(main())
