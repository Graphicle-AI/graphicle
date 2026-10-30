#!/usr/bin/env python3
"""Line-level recall on the whole-repository questions: does the context hold the lines each fix changes?

usage:
  lines.py gold --work WORK --questions DIR --diffs DIR --out FILE [--repos a,b] [--fetch] [--heldout]
      every question's gold lines, from its pull request's diff placed on the snapshot S: DIR/<repo>/<pr>.diff
      (fetched from github.com when missing and --fetch is given), restricted to the question's gold files.
      Each hunk is placed where its pre-image (context and '-' lines) matches the file at S exactly, at the
      offset nearest its header's line (after the previous hunk's offset), as `git apply` places a hunk
      without fuzz; then E1L's rule (e1l.py gold_lines) gives the gold lines on S: every '-' line, and for a
      hunk without '-' lines the line before each run of '+' lines. Every '-' line is checked against S.
      A question with a hunk that does not place, or without its diff, is dropped, with the reason.
      The held-out repositories are refused unless --heldout is given: they are measured, never tuned on.
  lines.py run --work WORK --questions DIR --texts DIR --gold FILE --graphicle BIN --data ROOT --out DIR
               [--tokenizer T] [--budgets 16K,...] [--repos a,b] [--arms pack,trunc,bm25] [--jobs 8]
               [--env NAME=VALUE ...] [--heldout]
      every question at every budget, one row per arm: `graphicle pack` (standard layout, JSON) on the index
      in ROOT/<repo>, truncation (tokens.py's order, whole files) and BM25 (bm25.py, whole files). For each
      gold file: inside at file level (the pack's `included` list, as recall.py counts it), and its gold lines
      inside: for the pack those a text chunk's line range covers (whole files and excerpts; lines shown only
      in an outline are counted apart, never inside), for truncation and BM25 all of them when the file is in.
      --env passes settings to the pack process (development switches of a development build).
  lines.py compare --base FILE --new FILE [--md OUT] [--json OUT] [--arm pack]
      two builds' rows on the same questions: per budget, pooled and per repository, file recall and (a), (b), (c)
      for each build and the change, paired by question, with 95% bootstrap percentile intervals (2,000 resamples
      of questions, seed 1 per cell; (c) resamples questions and pools their files).
  lines.py report --rows FILE [--base FILE] [--md OUT] [--json OUT] [--arm pack]
      per repository and budget, and pooled: file recall (the mean share of gold files inside, recall.py's
      number), and over the questions with gold lines (a) the share with every gold line inside, (b) the mean
      share of gold lines inside, (c) the share of gold files whose gold lines are all inside. With --base,
      each cell's change against the base rows and the keep rule that tuned 0.1.10's pack (keep_rule).
"""
import argparse
import json
import os
import re
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.join(HERE, "..", "swe-bench-pro"))
from e1l import HUNK, covered, file_lines, file_sections, gold_lines, pack_view  # noqa: E402
from mine import fetch_diff  # noqa: E402
from pack_arm import pack_command, run_measured  # noqa: E402
from tokens import Counter, load_manifest, truncation_order  # noqa: E402

DEV = ["kubernetes", "home-assistant", "angular", "keycloak", "godot"]
HELDOUT = ["pytorch", "vscode", "elasticsearch"]
BUDGETS = {"16K": 16384, "32K": 32768, "64K": 65536, "128K": 131072, "256K": 262144, "512K": 524288,
           "1M": 1015808}
SLUG = {"kubernetes": "kubernetes/kubernetes", "home-assistant": "home-assistant/core", "angular": "angular/angular",
        "keycloak": "keycloak/keycloak", "godot": "godotengine/godot", "pytorch": "pytorch/pytorch",
        "vscode": "microsoft/vscode", "elasticsearch": "elastic/elasticsearch"}


def allowed(repos, heldout):
    for r in repos:
        if r in HELDOUT and not heldout:
            raise SystemExit(f"{r} is held out: its gold is read only with --heldout, to measure it, never to tune")
        if r not in DEV and r not in HELDOUT:
            raise SystemExit(f"unknown repository {r}")


def questions(qdir, name):
    return [json.loads(x) for x in open(os.path.join(qdir, f"questions-{name}.jsonl")) if x.strip()]


def strip_cr(s):
    return s[:-1] if s.endswith("\r") else s


def place(lines, at_s):
    """The section's lines with each hunk header's old start moved to where its pre-image matches `at_s` (the
    file's lines at S), and the offsets; None and the reason when a hunk does not match anywhere."""
    out, offsets, i, n, shift, floor = [], [], 0, len(lines), 0, 0
    while i < n:
        m = HUNK.match(lines[i])
        if not m:
            out.append(lines[i])
            i += 1
            continue
        a, alen = int(m.group(1)), 1 if m.group(2) is None else int(m.group(2))
        blen = 1 if m.group(4) is None else int(m.group(4))
        j, old_left, new_left, pre = i + 1, alen, blen, []
        while j < n and (old_left > 0 or new_left > 0):
            line = lines[j]
            if line.startswith("\\"):
                j += 1
                continue
            tag = line[:1]
            if tag == " " or line == "":
                pre.append(strip_cr(line[1:]))
                old_left, new_left = old_left - 1, new_left - 1
            elif tag == "-":
                pre.append(strip_cr(line[1:]))
                old_left -= 1
            elif tag == "+":
                new_left -= 1
            else:
                break
            j += 1
        if not pre:  # an insertion without context: it stays where its header puts it
            off = shift
            if a + off > len(at_s) or a + off < 0:
                return None, f"insertion point {a} outside the file"
        else:
            want, k, found = a - 1 + shift, len(pre), None
            for d in range(0, len(at_s) + 1):
                for pos in ((want - d, want + d) if d else (want,)):
                    if floor <= pos <= len(at_s) - k and at_s[pos:pos + k] == pre:
                        found = pos
                        break
                if found is not None:
                    break
            if found is None:
                return None, f"hunk at line {a} does not match the file at S"
            off = found - (a - 1)
            floor = found + k
        shift = off
        offsets.append(off)
        hdr = lines[i]
        out.append(f"@@ -{a + off},{alen} +{m.group(3)},{blen} @@" + hdr[m.end():])
        out.extend(lines[i + 1:j])
        i = j
    return out, offsets


def cmd_gold(a):
    repos = a.repos.split(",")
    allowed(repos, a.heldout)
    work = os.path.abspath(a.work)
    counts, rows = {}, []
    for name in repos:
        c = counts.setdefault(name, {"questions": 0, "placed": 0, "dropped": {}, "gold_files": 0, "gold_lines": 0,
                                     "files_without_lines": 0, "minus_checked": 0, "minus_mismatch": 0,
                                     "hunks": 0, "hunks_moved": 0})
        src = os.path.join(work, "src", name)
        for q in questions(a.questions, name):
            c["questions"] += 1
            pr = int(q["pr_url"].rstrip("/").rsplit("/", 1)[1])
            d = os.path.join(a.diffs, name)
            os.makedirs(d, exist_ok=True)
            path = os.path.join(d, f"{pr}.diff")
            if not os.path.exists(path) and a.fetch:
                fetch_diff(SLUG[name], pr, path)
            rec = {"id": q["id"], "repo": name, "pr": pr, "files": {}, "drop": None}
            if not os.path.exists(path):
                rec["drop"] = "no diff"
            else:
                text = open(path, encoding="utf-8", errors="replace").read()
                secs = {}
                for old, status, lines in file_sections(text):
                    if old in q["gold"] and status != "added":
                        secs.setdefault(old, []).append((status, lines))
                for g in q["gold"]:
                    at_s, _nb = file_lines(os.path.join(src, g)) if os.path.exists(os.path.join(src, g)) else (None, 0)
                    if at_s is None:
                        rec["drop"] = f"{g} is not at S"
                        break
                    if g not in secs:
                        rec["drop"] = f"{g} has no section in the diff"
                        break
                    glines, minus, hunks, offs = set(), {}, 0, []
                    for _status, lines in secs[g]:
                        placed, info = place(lines, at_s)
                        if placed is None:
                            rec["drop"] = f"{g}: {info}"
                            break
                        gl, mt, h = gold_lines(placed)
                        glines |= set(gl)
                        minus.update(mt)
                        hunks += h
                        offs += info
                    if rec["drop"]:
                        break
                    mism = [ln for ln, t in minus.items() if ln > len(at_s) or at_s[ln - 1] != strip_cr(t)]
                    rec["files"][g] = {"lines": sorted(glines), "hunks": hunks, "offsets": offs,
                                       "minus": len(minus), "mismatch": mism}
            if rec["drop"]:
                why = ("no diff" if rec["drop"] == "no diff" else "not at S" if "is not at S" in rec["drop"]
                       else "no section" if "has no section" in rec["drop"] else "not placed")
                c["dropped"][why] = c["dropped"].get(why, 0) + 1
                rec["files"] = {}
            else:
                c["placed"] += 1
                for f in rec["files"].values():
                    c["gold_files"] += 1
                    c["gold_lines"] += len(f["lines"])
                    c["files_without_lines"] += 0 if f["lines"] else 1
                    c["minus_checked"] += f["minus"]
                    c["minus_mismatch"] += len(f["mismatch"])
                    c["hunks"] += f["hunks"]
                    c["hunks_moved"] += sum(1 for o in f["offsets"] if o)
            rows.append(rec)
    with open(a.out, "w") as f:
        for r in rows:
            f.write(json.dumps(r, sort_keys=True) + "\n")
    with open(a.out + ".counts.json", "w") as f:
        json.dump(counts, f, indent=1, sort_keys=True)
    for name, c in counts.items():
        print(f"{name}: {json.dumps(c, sort_keys=True)}")


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


def row_of(q, name, arm, b, per, tokens, nfiles, placed, extra=None):
    """One row: file-level inside (recall.py's recall and all) and, for a placed question, its line metrics."""
    have = [f for f in per if f["n_lines"]]
    total = sum(f["n_lines"] for f in have)
    inside = sum(f["inside_lines"] for f in have)
    nin = sum(1 for f in per if f["in_context"])
    lined = placed and total > 0
    r = {"id": q["id"], "repo": name, "arm": arm, "budget": b, "budget_tokens": BUDGETS[b], "tokens": tokens,
         "files_in_context": nfiles, "gold_n": len(q["gold"]), "file_inside": nin, "recall": nin / len(q["gold"]),
         "all": int(nin == len(q["gold"])), "names_gold_path": q["names_gold_path"], "placed": placed,
         "gold_lines": total if lined else None, "lines_inside": inside if lined else None,
         "line_share": inside / total if lined else None, "all_lines": int(inside == total) if lined else None,
         "files_all_lines": sum(1 for f in have if f["inside_lines"] == f["n_lines"]) if lined else 0,
         "files_with_lines": len(have) if lined else 0, "files": per}
    if extra:
        r.update(extra)
    return r


def cmd_run(a):
    repos = a.repos.split(",")
    allowed(repos, a.heldout)
    work = os.path.abspath(a.work)
    budgets = a.budgets.split(",")
    arms = a.arms.split(",")
    os.makedirs(a.out, exist_ok=True)
    gold = {r["id"]: r for r in map(json.loads, open(a.gold))}
    env = dict(os.environ, RAYON_NUM_THREADS=os.environ.get("RAYON_NUM_THREADS", "6"))
    for kv in a.env or []:
        k, v = kv.split("=", 1)
        env[k] = v
    out_rows = []
    for name in repos:
        qs = questions(a.questions, name)
        texts = {json.loads(x)["id"]: json.loads(x)["text"] for x in open(os.path.join(a.texts, f"texts-{name}.jsonl"))}

        def gold_of(q):
            g = gold.get(q["id"])
            placed = bool(g and not g["drop"])
            return {p: (g["files"][p]["lines"] if placed else []) for p in q["gold"]}, placed
        if "trunc" in arms:
            tsets, _c = truncation_sets(work, name, budgets)
            for q in qs:
                gl, placed = gold_of(q)
                for b in budgets:
                    inc, used = tsets[b]
                    per = [{"path": p, "n_lines": len(gl[p]), "inside_lines": len(gl[p]) if p in inc else 0,
                            "in_context": p in inc, "whole": p in inc} for p in q["gold"]]
                    out_rows.append(row_of(q, name, "trunc", b, per, used, len(inc), placed))
        if "bm25" in arms:
            from bm25 import build_bm25
            counter = Counter(a.tokenizer)
            cache = {}
            for q in qs:
                gl, placed = gold_of(q)
                text = texts[q["id"]]
                for b in budgets:
                    _t, inc, used = build_bm25(work, name, text, BUDGETS[b], counter, cache, reuse=True)
                    ranked = cache[(name, text)][0]
                    rk = {p: i + 1 for i, (p, _s) in enumerate(ranked[:5000])}
                    inc = set(inc)
                    per = [{"path": p, "n_lines": len(gl[p]), "inside_lines": len(gl[p]) if p in inc else 0,
                            "in_context": p in inc, "whole": p in inc, "rank": rk.get(p)}
                           for p in q["gold"]]
                    out_rows.append(row_of(q, name, "bm25", b, per, used, len(inc), placed))
                cache.clear()
            print(f"{name} bm25: done", flush=True)
        if "pack" in arms:
            data = os.path.join(a.data, name)
            src = os.path.join(work, "src", name)

            def one(job):
                q, b = job
                cmd = pack_command(a.graphicle, src, data, texts[q["id"]], BUDGETS[b], a.tokenizer, "json",
                                   "standard")
                t0 = time.time()
                for attempt in (1, 2):
                    try:
                        p = subprocess.run(cmd, capture_output=True, text=True, timeout=1800, env=env)
                    except subprocess.TimeoutExpired:
                        p = None
                    if p is not None and p.returncode == 0:
                        return job, json.loads(p.stdout), time.time() - t0
                return job, {"error": (p.stderr.strip()[-300:] if p is not None else "timeout")}, time.time() - t0
            jobs = [(q, b) for q in qs for b in budgets]
            with ThreadPoolExecutor(a.jobs) as ex:
                got = list(ex.map(one, jobs))
            for (q, b), pj, secs in got:
                gl, placed = gold_of(q)
                if pj.get("error"):
                    out_rows.append({"id": q["id"], "repo": name, "arm": "pack", "budget": b, "error": pj["error"]})
                    continue
                view = pack_view(pj, q["gold"])
                inc = {f["path"]: f for f in pj.get("included") or []}
                om = {f["path"]: f for f in pj.get("omitted") or []}
                per = []
                for p in q["gold"]:
                    v = view[p]
                    ins = [ln for ln in gl[p] if covered(ln, v["ranges"])]
                    f = {"path": p, "n_lines": len(gl[p]), "inside_lines": len(ins),
                         "outline_lines": sum(1 for ln in gl[p] if ln in v["outline"] and ln not in ins),
                         "in_context": p in inc, "whole": v["whole"], "ranges": v["ranges"], "text": bool(v["ranges"]),
                         "outline": bool(v["outline"]), "rank": (inc.get(p) or om.get(p) or {}).get("rank"),
                         "pack_tokens": (inc.get(p) or {}).get("tokens"), "omitted_listed": p in om}
                    if v["ranges"] and len(ins) < len(gl[p]):
                        f["min_dist"] = min(min(abs(ln - s), abs(ln - e)) for ln in gl[p] if ln not in ins
                                            for s, e in v["ranges"])
                    per.append(f)
                out_rows.append(row_of(q, name, "pack", b, per, pj.get("tokens"), len(inc), placed,
                                       {"seconds": round(secs, 2), "sections": {s.get("name"): s.get("tokens")
                                                                                for s in pj.get("sections") or []}}))
            print(f"{name} pack: {len(jobs)} packs, "
                  f"{sum(1 for x in got if x[1].get('error'))} errors", flush=True)
    with open(os.path.join(a.out, "lines-rows.jsonl"), "w") as f:
        for r in out_rows:
            f.write(json.dumps(r, sort_keys=True) + "\n")
    print(f"rows: {len(out_rows)}, errors: {sum(1 for r in out_rows if r.get('error'))}")


def cells(rows, arm):
    """{(repo or 'pooled', budget): metrics} of one arm's rows."""
    out = {}
    for r in rows:
        if r.get("arm") != arm or r.get("error"):
            continue
        for scope in (r["repo"], "pooled"):
            c = out.setdefault((scope, r["budget"]), {"n": 0, "recall": 0.0, "all": 0, "nl": 0, "all_lines": 0,
                                                      "line_share": 0.0, "files_all": 0, "files_lines": 0, "ids": {}})
            c["n"] += 1
            c["recall"] += r["recall"]
            c["all"] += r["all"]
            if r.get("gold_lines"):
                c["nl"] += 1
                c["all_lines"] += r["all_lines"]
                c["line_share"] += r["line_share"]
                c["files_all"] += r["files_all_lines"]
                c["files_lines"] += r["files_with_lines"]
            c["ids"][r["id"]] = (r["recall"], r.get("all_lines"))
    for c in out.values():
        n, nl = c["n"], c["nl"]
        c["recall"] = 100 * c["recall"] / n if n else None
        c["all"] = 100 * c["all"] / n if n else None
        c["a"] = 100 * c["all_lines"] / nl if nl else None
        c["b"] = 100 * c["line_share"] / nl if nl else None
        c["c"] = 100 * c["files_all"] / c["files_lines"] if c["files_lines"] else None
    return out


def order_budgets(keys):
    return sorted({b for _s, b in keys}, key=lambda b: BUDGETS[b])


def fmt(x, d=1):
    return "-" if x is None else f"{x:.{d}f}"


EPS = 1e-9  # a change exactly at a bound meets it (shares are sums of floats)


def keep_rule(cur, base, budgets):
    """The keep rule that tuned 0.1.10's pack, on the pack arm (cells of cur and base). A change is kept when the
    pooled (a) rises by 0.5 points or more on the mean over the budgets, falls by no more than 1.0 point at any
    budget, pooled file recall falls by no more than 1.0 point at 16K and 32K and 0.5 points from 64K, and no
    repository's file recall falls by more than 2.5 points from 64K. Returns (kept, reasons, mean change)."""
    reasons, ok = [], True
    gain = [cur[("pooled", b)]["a"] - base[("pooled", b)]["a"] for b in budgets]
    mean_gain = sum(gain) / len(gain)
    if mean_gain < 0.5 - EPS:
        ok = False
        reasons.append(f"line (a) mean change {mean_gain:+.2f} < +0.5")
    for b, g in zip(budgets, gain):
        if g < -1.0 - EPS:
            ok = False
            reasons.append(f"line (a) at {b} {g:+.1f} < -1.0")
    for b in budgets:
        d = cur[("pooled", b)]["recall"] - base[("pooled", b)]["recall"]
        tol = 1.0 if b in ("16K", "32K") else 0.5
        if d < -tol - EPS:
            ok = False
            reasons.append(f"file recall pooled at {b} {d:+.2f} < -{tol}")
    for (scope, b), c in cur.items():
        if scope == "pooled" or b in ("16K", "32K"):
            continue
        d = c["recall"] - base[(scope, b)]["recall"]
        if d < -2.5 - EPS:
            ok = False
            reasons.append(f"file recall {scope} at {b} {d:+.2f} < -2.5")
    return ok, reasons, mean_gain


def cmd_report(a):
    rows = [json.loads(x) for x in open(a.rows)]
    cur = cells(rows, a.arm)
    base = cells([json.loads(x) for x in open(a.base)], a.arm) if a.base else None
    budgets = order_budgets(cur.keys())
    scopes = [s for s in DEV + HELDOUT if any(k[0] == s for k in cur)] + ["pooled"]
    lines = []
    errs = sum(1 for r in rows if r.get("error") and r.get("arm") == a.arm)
    lines.append(f"rows: {a.rows} (arm {a.arm}; errors {errs})" + (f"; base {a.base}" if a.base else ""))
    for metric, title in (("recall", "file recall (mean share of gold files inside)"),
                          ("a", "(a) questions with every gold line inside"),
                          ("b", "(b) mean share of gold lines inside"),
                          ("c", "(c) gold files with all their gold lines inside")):
        lines.append("")
        lines.append(f"**{title}**" + (" (change against the base)" if base else ""))
        lines.append("")
        lines.append("| | " + " | ".join(budgets) + " |")
        lines.append("|---|" + "---|" * len(budgets))
        for s in scopes:
            vals = []
            for b in budgets:
                c = cur.get((s, b))
                v = c[metric] if c else None
                if base and c and (s, b) in base and base[(s, b)][metric] is not None and v is not None:
                    vals.append(f"{fmt(v)} ({v - base[(s, b)][metric]:+.1f})")
                else:
                    vals.append(fmt(v))
            n = cur.get((s, budgets[0]), {})
            lines.append(f"| {s} (n={n.get('n')}, lines {n.get('nl')}) | " + " | ".join(vals) + " |")
    out = {"rows": a.rows, "base": a.base, "cells": {f"{s}|{b}": {k: v for k, v in c.items() if k != "ids"}
                                                    for (s, b), c in cur.items()}}
    if base:
        kept, reasons, mean_gain = keep_rule(cur, base, budgets)
        moved = {}
        for b in budgets:
            ids = cur[("pooled", b)]["ids"]
            bids = base[("pooled", b)]["ids"]
            up = sum(1 for i, (_r, al) in ids.items() if al == 1 and bids.get(i, (0, None))[1] == 0)
            down = sum(1 for i, (_r, al) in ids.items() if al == 0 and bids.get(i, (0, None))[1] == 1)
            fup = sum(1 for i, (r, _al) in ids.items() if r > bids.get(i, (r, None))[0])
            fdown = sum(1 for i, (r, _al) in ids.items() if r < bids.get(i, (r, None))[0])
            moved[b] = {"lines_up": up, "lines_down": down, "files_up": fup, "files_down": fdown}
        lines.append("")
        lines.append(f"**Keep rule:** {'KEEP' if kept else 'not kept'}; mean (a) change {mean_gain:+.2f}"
                     + ("" if kept else "; " + "; ".join(reasons)))
        lines.append("")
        lines.append("Questions that changed (pooled): " + "; ".join(
            f"{b} lines +{m['lines_up']}/-{m['lines_down']}, files +{m['files_up']}/-{m['files_down']}"
            for b, m in moved.items()))
        out.update({"kept": kept, "reasons": reasons, "mean_a_change": mean_gain, "moved": moved})
    text = "\n".join(lines) + "\n"
    print(text)
    if a.md:
        open(a.md, "w").write(text)
    if a.json:
        json.dump(out, open(a.json, "w"), indent=1, sort_keys=True)


def boot(xs, rng, n=2000):
    if not xs:
        return None
    vals = sorted(sum(xs[rng.randrange(len(xs))] for _ in xs) / len(xs) for _ in range(n))
    return [vals[int(0.025 * n)], vals[min(n - 1, int(0.975 * n))]]


def boot_ratio(pairs, rng, n=2000):
    """95% interval of sum(x)/sum(d) over resampled questions; pairs (x, d)."""
    if not pairs:
        return None
    vals = []
    for _ in range(n):
        s = [pairs[rng.randrange(len(pairs))] for _ in pairs]
        d = sum(y for _x, y in s)
        vals.append(sum(x for x, _y in s) / d if d else 0.0)
    vals.sort()
    return [vals[int(0.025 * n)], vals[min(n - 1, int(0.975 * n))]]


def cmd_compare(a):
    import random
    base = {(r["id"], r["budget"]): r for r in map(json.loads, open(a.base))
            if r.get("arm") == a.arm and not r.get("error")}
    new = {(r["id"], r["budget"]): r for r in map(json.loads, open(a.new))
           if r.get("arm") == a.arm and not r.get("error")}
    keys = sorted(set(base) & set(new))
    budgets = sorted({b for _i, b in keys}, key=lambda b: BUDGETS[b])
    repos = [x for x in DEV + HELDOUT if any(new[k]["repo"] == x for k in keys)]
    out, md = {"base": a.base, "new": a.new, "cells": {}}, []
    md.append(f"base {a.base}; new {a.new}; question-budget pairs in both: {len(keys)} (base only "
              f"{len(set(base) - set(new))}, new only {len(set(new) - set(base))})")
    for scope in ["pooled"] + repos:
        for b in budgets:
            ks = [k for k in keys if k[1] == b and (scope == "pooled" or new[k]["repo"] == scope)]
            if not ks:
                continue
            cell = {"n": len(ks)}
            for metric in ("recall", "a", "b"):
                field = {"recall": "recall", "a": "all_lines", "b": "line_share"}[metric]
                ks2 = [k for k in ks if base[k].get(field) is not None and new[k].get(field) is not None]
                xb = [float(base[k][field]) for k in ks2]
                xn = [float(new[k][field]) for k in ks2]
                rng = random.Random(1)
                cell[metric] = {"n": len(ks2), "base": 100 * sum(xb) / len(xb) if xb else None,
                                "new": 100 * sum(xn) / len(xn) if xn else None,
                                "change": 100 * (sum(xn) - sum(xb)) / len(xb) if xb else None,
                                "change_ci": [100 * v for v in boot([y - x for x, y in zip(xb, xn)], rng) or []]}
            ks3 = [k for k in ks if base[k].get("files_with_lines")]
            pb = [(base[k]["files_all_lines"], base[k]["files_with_lines"]) for k in ks3]
            pn = [(new[k]["files_all_lines"], new[k]["files_with_lines"]) for k in ks3]
            tot = sum(d for _x, d in pb)
            rng = random.Random(1)
            cell["c"] = {"n": tot, "base": 100 * sum(x for x, _d in pb) / tot if tot else None,
                         "new": 100 * sum(x for x, _d in pn) / tot if tot else None,
                         "change": 100 * (sum(x for x, _d in pn) - sum(x for x, _d in pb)) / tot if tot else None,
                         "change_ci": [100 * v for v in boot_ratio([(y[0] - x[0], x[1]) for x, y in zip(pb, pn)], rng)
                                       or []]}
            out["cells"][f"{scope}|{b}"] = cell
    for metric, title in (("recall", "file recall, %"), ("a", "(a) every gold line inside, %"),
                          ("b", "(b) mean share of gold lines inside, %"),
                          ("c", "(c) gold files with all their gold lines inside, % (pooled)")):
        md += ["", f"**{title}: base / new / change (paired 95% interval)**", "",
               "| | " + " | ".join(budgets) + " |", "|---|" + "---|" * len(budgets)]
        for scope in ["pooled"] + repos:
            vals = []
            for b in budgets:
                c = out["cells"].get(f"{scope}|{b}", {}).get(metric)
                if not c or c["base"] is None:
                    vals.append("-")
                    continue
                ci_ = c["change_ci"]
                vals.append(f"{c['base']:.1f} / {c['new']:.1f} / {c['change']:+.1f}"
                            + (f" ({ci_[0]:+.1f} to {ci_[1]:+.1f})" if ci_ else ""))
            md.append(f"| {scope} | " + " | ".join(vals) + " |")
    text = "\n".join(md) + "\n"
    print(text)
    if a.md:
        open(a.md, "w").write(text)
    if a.json:
        json.dump(out, open(a.json, "w"), indent=1, sort_keys=True)


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    g = sub.add_parser("gold")
    g.add_argument("--work", required=True)
    g.add_argument("--questions", required=True)
    g.add_argument("--diffs", required=True)
    g.add_argument("--out", required=True)
    g.add_argument("--repos", default=",".join(DEV))
    g.add_argument("--fetch", action="store_true")
    g.add_argument("--heldout", action="store_true")
    r = sub.add_parser("run")
    r.add_argument("--work", required=True)
    r.add_argument("--questions", required=True)
    r.add_argument("--texts", required=True)
    r.add_argument("--gold", required=True)
    r.add_argument("--graphicle")
    r.add_argument("--data")
    r.add_argument("--out", required=True)
    r.add_argument("--tokenizer")
    r.add_argument("--budgets", default=",".join(BUDGETS))
    r.add_argument("--repos", default=",".join(DEV))
    r.add_argument("--arms", default="pack")
    r.add_argument("--jobs", type=int, default=8)
    r.add_argument("--env", action="append")
    r.add_argument("--heldout", action="store_true")
    c = sub.add_parser("compare")
    c.add_argument("--base", required=True)
    c.add_argument("--new", required=True)
    c.add_argument("--md")
    c.add_argument("--json")
    c.add_argument("--arm", default="pack")
    p = sub.add_parser("report")
    p.add_argument("--rows", required=True)
    p.add_argument("--base")
    p.add_argument("--md")
    p.add_argument("--json")
    p.add_argument("--arm", default="pack")
    a = ap.parse_args()
    {"gold": cmd_gold, "run": cmd_run, "report": cmd_report, "compare": cmd_compare}[a.cmd](a)


if __name__ == "__main__":
    main()
