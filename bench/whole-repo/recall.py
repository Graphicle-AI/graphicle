#!/usr/bin/env python3
"""Context recall without a model: are the gold files inside the pack and inside truncation?

usage: recall.py --work WORKDIR --questions DIR --texts DIR --graphicle BIN --out DIR
                 [--budgets 64K,128K,256K,512K,1M] [--repos a,b] [--standin] [--tokenizer FILE] [--jobs 4]
                 [--layouts standard,cache0.9,cache0.95,stable]

For every question and budget: the pack's included files and truncation's (tokens.py truncation_order
over the manifest, the same files build_truncation takes), and for each: recall (the share of the gold
files inside), any (some gold file inside) and all (every gold file inside). Pass the same --tokenizer
that tokens.py sizes counted the manifests with, so both arms fill the same budget.

For the pack, every gold file gets a status at every budget:
  inside         the pack holds some of its text
  manifest-only  left out, but named in the pack's manifest as a top-ranked file left out
  ranked-cut     ranked by the retrieval (its rank is seen in any of the question's packs) but left
                 out, and not named in the manifest
  unranked       never among the ranked files of any of the question's packs (the ranking does not
                 depend on the budget; at the largest budgets nearly every ranked file fits)
and each call's wall seconds and peak memory (resident set of the pack process) are recorded.
--standin uses `graphicle context` in place of `graphicle pack` and labels every row so: those rows
test the plumbing and are not results.

--layouts runs the pack once per layout, each as its own arm: standard (`--layout standard`, arm
pack), cacheF (`--layout cache --prefix-share F`, arm pack-cache for F = 0.9, else pack-cacheF) and
stable (`--layout cache --prefix-share 1.0`, arm pack-stable). Each pack row then also records the
pack's layout fields: its prefix and suffix tokens, prefix_sha256 and whether the prefix comes first
and matches its hash (prefix_ok), and the question's own tokens; the summary gives their means, the
number of distinct prefix hashes per repository, budget and layout (1 when every question shares one
prefix) and how many rows passed the prefix check. Without --layouts the pack runs once, with no
--layout option, as builds before the cache layout need.

Writes OUT/recall-rows.jsonl (per question, budget and arm) and OUT/recall-summary.json (means per
repository, budget and arm, with and without the questions whose text names a gold file path).
"""
import argparse
import json
import os
import re
import statistics
import sys
from concurrent.futures import ThreadPoolExecutor

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from pack_arm import pack_context, pack_json  # noqa: E402
from run import BUDGETS, PROMPT  # noqa: E402
from tokens import Counter, load_manifest, truncation_order  # noqa: E402

STATUSES = ["inside", "manifest-only", "ranked-cut", "unranked"]


def layout_spec(spec):
    """(arm label, --layout, --prefix-share) for one --layouts item."""
    if spec == "standard":
        return "pack", "standard", None
    if spec == "stable":
        return "pack-stable", "cache", 1.0
    m = re.fullmatch(r"cache(0?\.\d+|1(?:\.0+)?)", spec)
    if not m:
        raise SystemExit(f"unknown layout {spec!r}: use standard, cacheF (such as cache0.9) or stable")
    share = float(m.group(1))
    return ("pack-cache" if abs(share - 0.9) < 1e-9 else f"pack-cache{share:g}"), "cache", share


def truncation_sets(work, name, budgets):
    rows, counter = load_manifest(work, name)
    order = truncation_order(name, rows)
    out = {}
    for b in budgets:
        used, inc = 0, set()
        for _r, p, c, w in order:
            if used + c + w <= BUDGETS[b]:
                inc.add(p)
                used += c + w
        out[b] = (inc, used)
    return out, counter


def measure(gold, included):
    inside = [g for g in gold if g in included]
    return {"recall": len(inside) / len(gold), "any": int(bool(inside)), "all": int(len(inside) == len(gold))}


def pack_rows(a, work, name, qs, texts, budgets, arm="pack", layout=None, share=None, qtokens=None):
    """The real pack: every question at every budget, then each gold file's status."""
    def one(job):
        q, b = job
        try:
            return job, pack_json(a.graphicle, os.path.join(work, "src", name), os.path.join(work, "data", name),
                                  texts[q["id"]], BUDGETS[b], a.tokenizer, layout, share)
        except Exception as e:
            return job, {"error": f"{type(e).__name__}: {str(e)[:200]}"}

    jobs = [(q, b) for q in qs for b in budgets]
    with ThreadPoolExecutor(a.jobs) as ex:
        got = {(q["id"], b): s for (q, b), s in ex.map(one, jobs)}
    rows = []
    for q in qs:
        rank = {}
        for b in budgets:
            s = got[(q["id"], b)]
            if s.get("error"):
                continue
            rank.update({p: r for p, r in s["included"].items() if r})
            rank.update({p: r for p, r in s["omitted"] if r})
            rank.update(s["left_out_listed"])
        for b in budgets:
            s = got[(q["id"], b)]
            row = {"id": q["id"], "repo": name, "budget": b, "names_gold_path": q["names_gold_path"], "arm": arm,
                   "source": "graphicle pack"}
            if s.get("error"):
                rows.append(dict(row, error=s["error"]))
                continue
            status = {}
            for g in q["gold"]:
                if g in s["included"]:
                    status[g] = "inside"
                elif g in s["left_out_listed"]:
                    status[g] = "manifest-only"
                elif g in rank:
                    status[g] = "ranked-cut"
                else:
                    status[g] = "unranked"
            row.update(measure(q["gold"], set(s["included"])))
            row.update({"status": status, "gold_rank": {g: rank.get(g) for g in q["gold"]},
                        "gold_inside_by_rank": {g: s["included"].get(g) for g in q["gold"] if g in s["included"]},
                        "tokens": s["tokens"], "tokenizer": s["tokenizer"], "files": len(s["included"]),
                        "seconds": s["seconds"], "peak_mib": s["peak_mib"], "sections": s["sections"],
                        "ranked_files_seen": len(rank)})
            if layout:
                row.update({k: s.get(k) for k in ("layout", "prefix_share", "prefix_tokens", "prefix_sha256",
                                                  "prefix_ok", "suffix_tokens", "parts")})
                row["question_tokens"] = (qtokens or {}).get(q["id"])
            rows.append(row)
    return rows


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True)
    ap.add_argument("--questions", required=True)
    ap.add_argument("--texts", required=True)
    ap.add_argument("--graphicle", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--budgets", default="64K,128K,256K,512K,1M")
    ap.add_argument("--repos", default="kubernetes,home-assistant")
    ap.add_argument("--standin", action="store_true")
    ap.add_argument("--tokenizer")
    ap.add_argument("--jobs", type=int, default=4)
    ap.add_argument("--layouts", help="pack layouts, each its own arm: standard, cacheF (such as cache0.9), stable")
    a = ap.parse_args()
    work = os.path.abspath(a.work)
    budgets = a.budgets.split(",")
    os.makedirs(a.out, exist_ok=True)
    pack_label = "pack-standin" if a.standin else "pack"
    layouts = [layout_spec(x) for x in a.layouts.split(",")] if a.layouts else None
    if layouts and a.standin:
        raise SystemExit("--layouts needs the real pack, not --standin")
    qcounter = Counter(a.tokenizer) if layouts else None
    rows = []
    for name in a.repos.split(","):
        qs = [json.loads(l) for l in open(os.path.join(a.questions, f"questions-{name}.jsonl")) if l.strip()]
        texts = {json.loads(l)["id"]: json.loads(l)["text"] for l in open(os.path.join(a.texts, f"texts-{name}.jsonl"))}
        tsets, counter = truncation_sets(work, name, budgets)
        for q in qs:
            for b in budgets:
                inc, used = tsets[b]
                rows.append({"id": q["id"], "repo": name, "budget": b, "names_gold_path": q["names_gold_path"],
                             "arm": "truncation", "source": f"truncation ({counter})", "tokens": used,
                             "files": len(inc), **measure(q["gold"], inc)})
        if a.standin:
            def one(job):
                q, b = job
                row = {"id": q["id"], "repo": name, "budget": b, "names_gold_path": q["names_gold_path"],
                       "arm": pack_label}
                try:
                    p = pack_context(a.graphicle, os.path.join(work, "src", name), os.path.join(work, "data", name),
                                     texts[q["id"]], BUDGETS[b], None, True, want_text=False)
                    return dict(row, source=p["source"], tokens=p["tokens"], files=len(p["included"]),
                                seconds=round(p["seconds"], 2), **measure(q["gold"], set(p["included"])))
                except Exception as e:
                    return dict(row, error=f"{type(e).__name__}: {str(e)[:200]}")
            with ThreadPoolExecutor(a.jobs) as ex:
                rows.extend(ex.map(one, [(q, b) for q in qs for b in budgets]))
        elif layouts:
            # The prompt's part after the context: the template and the issue (never inside the pack).
            qtokens = {q["id"]: qcounter.count(PROMPT.format(context="", repo=q["repo"], sha=q["S"],
                                                             issue=texts[q["id"]])) for q in qs}
            for arm, layout, share in layouts:
                rows.extend(pack_rows(a, work, name, qs, texts, budgets, arm, layout, share, qtokens))
                print(f"{name} {arm}: done", flush=True)
        else:
            rows.extend(pack_rows(a, work, name, qs, texts, budgets))
        print(f"{name}: {len(qs) * len(budgets)} question-budget pairs", flush=True)
    with open(os.path.join(a.out, "recall-rows.jsonl"), "w") as f:
        for r in rows:
            f.write(json.dumps(r, sort_keys=True) + "\n")
    summary = {}
    for r in rows:
        if r.get("error"):
            continue
        for scope in ("all", "without_named"):
            if scope == "without_named" and r["names_gold_path"]:
                continue
            k = (r["repo"], r["budget"], r["arm"], scope)
            s = summary.setdefault(k, {"repo": r["repo"], "budget": r["budget"], "arm": r["arm"], "scope": scope,
                                       "n": 0, "recall": 0.0, "any": 0, "all": 0, "tokens": 0, "source": r["source"],
                                       "gold_status": {}, "seconds": [], "peak_mib": []})
            s["n"] += 1
            s["recall"] += r["recall"]
            s["any"] += r["any"]
            s["all"] += r["all"]
            s["tokens"] += r.get("tokens") or 0
            for st in (r.get("status") or {}).values():
                s["gold_status"][st] = s["gold_status"].get(st, 0) + 1
            if r.get("seconds") is not None:
                s["seconds"].append(r["seconds"])
            if r.get("peak_mib") is not None:
                s["peak_mib"].append(r["peak_mib"])
            if r.get("layout"):
                lay = s.setdefault("layout_rows", {"layout": r["layout"], "prefix_share": r.get("prefix_share"),
                                                   "prefix_tokens": [], "suffix_tokens": [], "question_tokens": [],
                                                   "prefix_shas": set(), "prefix_ok": 0})
                lay["prefix_tokens"].append(r.get("prefix_tokens") or 0)
                lay["suffix_tokens"].append(r.get("suffix_tokens") or 0)
                lay["question_tokens"].append(r.get("question_tokens") or 0)
                lay["prefix_shas"].add(r.get("prefix_sha256"))
                lay["prefix_ok"] += 1 if r.get("prefix_ok") else 0
    for s in summary.values():
        n = s["n"]
        s["recall"], s["any"], s["all"], s["tokens"] = s["recall"] / n, s["any"] / n, s["all"] / n, s["tokens"] / n
        for k in ("seconds", "peak_mib"):
            xs = sorted(s.pop(k))
            s[k] = ({"median": statistics.median(xs), "p90": xs[int(0.9 * (len(xs) - 1))], "max": xs[-1]}
                    if xs else None)
        lay = s.pop("layout_rows", None)
        if lay:
            s.update({"layout": lay["layout"], "prefix_share": lay["prefix_share"],
                      "prefix_tokens": statistics.mean(lay["prefix_tokens"]),
                      "prefix_tokens_min": min(lay["prefix_tokens"]),
                      "suffix_tokens": statistics.mean(lay["suffix_tokens"]),
                      "question_tokens": statistics.mean(lay["question_tokens"]),
                      "prefix_shas": len(lay["prefix_shas"]), "prefix_ok": lay["prefix_ok"]})
    errors = sum(1 for r in rows if r.get("error"))
    ordered = sorted(summary.values(), key=lambda s: (s["repo"], BUDGETS[s["budget"]], s["arm"], s["scope"]))
    with open(os.path.join(a.out, "recall-summary.json"), "w") as f:
        json.dump({"pack_source": pack_label, "tokenizer": a.tokenizer and os.path.basename(a.tokenizer),
                   "layouts": a.layouts, "errors": errors, "rows": ordered}, f, indent=1)
    for s in ordered:
        if s["scope"] == "all":
            extra = ""
            if s["arm"].startswith("pack") and s["seconds"]:
                extra = (f" status={s['gold_status']} sec={s['seconds']['median']:.1f}/{s['seconds']['max']:.1f}"
                         f" mib={s['peak_mib']['median']:.0f}/{s['peak_mib']['max']:.0f}")
            if s.get("layout"):
                extra += (f" prefix={s['prefix_tokens']:,.0f} suffix={s['suffix_tokens']:,.0f}"
                          f" prefix_shas={s['prefix_shas']} prefix_ok={s['prefix_ok']}/{s['n']}")
            print(f"{s['repo']:15s} {s['budget']:>5s} {s['arm']:13s} n={s['n']:3d} recall={s['recall']:.3f} "
                  f"any={s['any']:.3f} all={s['all']:.3f} tokens={s['tokens']:,.0f}{extra}")
    print(f"errors: {errors}")


if __name__ == "__main__":
    main()
