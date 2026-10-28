#!/usr/bin/env python3
"""The whole-repository benchmark's data file and report page.

usage: report.py --sizes SIZES.json [--sizes-estimate SIZES.json] [--counter-label TEXT] --index-dir DIR
                 [--score SCORE.json] [--recall LABEL=RECALL.json ...] [--texts DIR] [--layouts LAYOUTS.json]
                 --json OUT.json --md OUT.md [--test]

Reads the repository sizes (tokens.py sizes), the index records (index.py), the question funnel
(questions/funnel.json), the scores of the model runs (score.py), the context recall without a
model (recall.py), the registered hosted grids (estimate.py REGISTERED, priced with estimate.py's
defaults; --texts gives the issue sizes) and, for the cost section, the pack layouts' prefix and
suffix tokens (recall.py --layouts on the development repositories; without it, nominal shares), and
writes:
  OUT.json  every number the page and its charts show (docs/benchmarks/whole-repo.json)
  OUT.md    the page (docs/benchmarks/whole-repo.md): method, repositories, questions, results
Sections whose inputs are missing say so. Rows from `graphicle context` (the stand-in for testing the
plumbing) and runs on the mock server are refused unless --test is given, and a --test page says on
its first line that its numbers are test data.
"""
import argparse
import datetime
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from run import BUDGETS, PROMPT  # noqa: E402
import estimate  # noqa: E402

ARM_ORDER = ["pack@128K", "pack-cache@128K", "pack@512K", "pack@1M", "pack-cache@1M", "trunc@128K", "trunc@512K",
             "trunc@1M", "agent", "noctx"]
ARM_NAMES = {"pack": "Graphicle pack", "pack-cache": "Graphicle pack, cache layout",
             "pack-stable": "Graphicle pack, stable layout", "trunc": "Truncation", "agent": "Search-only agent",
             "noctx": "No context"}
MODEL_NAMES = {"muse-spark-1.3": "Muse Spark 1.3", "muse-wonder-1.0": "Muse Wonder",
               "meta-models/muse-spark-open": "Muse Spark Open", "muse-spark-1.2": "Muse Spark 1.2"}
PROVIDER_NAMES = {"meta": "Meta Model API, Responses API", "baseten": "Baseten Model APIs, chat completions"}
GRID_ARM_TEXT = {"noctx": "no context", "trunc": "truncation", "pack": "pack", "pack-cache": "cache-layout pack",
                 "pack-stable": "stable pack", "agent": "search-only agent"}


def arm_name(label):
    fam, _, b = label.partition("@")
    fam = fam.replace("-standin", "")
    return ARM_NAMES.get(fam, fam) + (f", {b}" if b else "")


COUNTER_LABEL = {}


def counter_text(name):
    if name in COUNTER_LABEL:
        return COUNTER_LABEL[name]
    if name.startswith("estimate:"):
        return "estimated as UTF-8 bytes / 4"
    return "counted with " + name.split(":", 1)[-1]


def pct(x):
    return "n/a" if x is None else f"{100 * x:.1f}%"


def ci(c, scale=100, unit="%"):
    return "" if not c else f" [{scale * c[0]:.1f}, {scale * c[1]:.1f}]" if unit == "%" else f" [{c[0]:.3f}, {c[1]:.3f}]"


def mtok(n):
    return f"{n / 1e6:.1f}M"


def build(a):
    cfg = json.load(open(os.path.join(HERE, "repos.json")))
    snaps = json.load(open(os.path.join(HERE, "snapshots.json")))
    funnel = json.load(open(os.path.join(HERE, "questions", "funnel.json")))
    sizes = json.load(open(a.sizes))
    estimate = json.load(open(a.sizes_estimate)) if a.sizes_estimate else None
    if a.counter_label:
        COUNTER_LABEL[sizes["counter"]] = a.counter_label
    data = {"generated": datetime.date.today().isoformat(), "test_data": bool(a.test),
            "counter": sizes["counter"], "counter_label": counter_text(sizes["counter"]),
            "estimate_counter": estimate and estimate["counter"], "snapshot_cutoff": cfg["snapshot_cutoff"],
            "merged_until": cfg["merged_until"], "selection_seed": cfg["selection_seed"],
            "prices": json.load(open(os.path.join(HERE, "prices.json"))), "repos": [], "runs": {}, "paired": [],
            "recall": None}
    for r in cfg["repos"]:
        n = r["name"]
        s = sizes["repos"][n]
        idx_path = os.path.join(a.index_dir, n + ".json")
        idx = json.load(open(idx_path)) if os.path.exists(idx_path) else {}
        files = sum(v["files"] for v in s["by_class"].values())
        data["repos"].append({
            "name": n, "slug": r["slug"], "role": r["role"], "S": snaps[n]["sha"], "S_date": snaps[n]["committed"],
            "tokens_all": s["tokens_all"], "tokens_without_vendored_generated": s["tokens_without_vendored_generated"],
            "tokens_nontest_source": s["tokens_nontest_source"], "text_files": files,
            "index_seconds": idx.get("wall_seconds"), "index_bytes": idx.get("data_bytes"),
            "index_peak_rss_mib": idx.get("peak_rss_mib"), "graphicle": idx.get("graphicle"),
            "funnel": funnel[n]})
        if estimate:
            e = estimate["repos"][n]
            data["repos"][-1]["estimate"] = {k: e[k] for k in ("tokens_all", "tokens_without_vendored_generated",
                                                               "tokens_nontest_source")}
    if a.score:
        sc = json.load(open(a.score))
        for label, run in sc["runs"].items():
            m = run["meta"]
            if (m.get("model") == "mock" or "stand-in" in (m.get("context_source") or "")) and not a.test:
                raise SystemExit(f"{label}: mock or stand-in run; pass --test to build a test page")
            data["runs"][label] = run
        data["paired"] = sc["paired"]
        data["by_role"] = by_role(data)
    data["recall"] = []
    for spec in a.recall or []:
        label, sep, path = spec.rpartition("=")
        path = path if sep else spec
        with open(path) as f:
            rc = json.load(f)
        if rc["pack_source"] != "pack" and not a.test:
            raise SystemExit("the recall file comes from the stand-in; pass --test to build a test page")
        data["recall"].append(dict(rc, label=label if sep else rc["pack_source"]))
    data["registered"] = registered_grids(a, cfg, data["prices"])
    data["cost"] = cost_section(a, cfg, data["prices"])
    return data


def registered_grids(a, cfg, prices):
    """The pre-registered hosted grids (estimate.REGISTERED), each with its estimated cost at the
    estimate's default assumptions."""
    roles = {r["name"]: r["role"] for r in cfg["repos"]}
    nq, qtok = estimate.load_questions(os.path.join(HERE, "questions"), a.texts, cfg)
    caps = json.load(open(os.path.join(HERE, "caps.json")))["usd"]
    agent = {"turns": 15, "turn_tokens": 2500, "out_per_turn": 800}
    out = []
    for name, desc, model, grid in estimate.REGISTERED:
        p = prices["models"][model]
        rows = estimate.grid_rows(grid, nq, qtok, roles, p, 4000, agent)
        items = []
        for item in grid.split(","):
            parts = item.split(":")
            arm, _, budget = parts[0].partition("@")
            first = int(parts[2]) if len(parts) > 2 else None
            names = estimate.repo_names(parts[1], roles)
            items.append({"arm": arm, "budget": budget or None, "repos": names, "first": first,
                          "questions": sum(min(nq[n], first) if first else nq[n] for n in names)})
        out.append({"grid": name, "description": desc, "model": model, "provider": p.get("provider"),
                    "cap_usd": caps.get(p.get("provider")), "items": items,
                    "estimate_cached_usd": round(sum(r["cached"] for r in rows), 2),
                    "estimate_uncached_usd": round(sum(r["uncached"] for r in rows), 2)})
    return out


COST_MODELS = ["muse-spark-1.3", "muse-wonder-1.0"]
COST_CONTEXTS = [("pack", "Graphicle pack, standard layout", None),
                 ("pack-cache0.5", "Graphicle pack, cache layout, 50% prefix", 0.5),
                 ("pack-cache0.7", "Graphicle pack, cache layout, 70% prefix", 0.7),
                 ("pack-cache", "Graphicle pack, cache layout, 90% prefix", 0.9),
                 ("pack-cache0.95", "Graphicle pack, cache layout, 95% prefix", 0.95),
                 ("pack-stable", "Graphicle pack, stable layout", 1.0), ("truncation", "Truncation", 1.0)]
COST_BUDGETS = ["128K", "256K", "512K", "1M"]
SESSION = 20
COST_LABEL = "computed at list prices, not billed"


def cost_section(a, cfg, prices):
    """Input cost per question with and without the prompt cache, computed at list prices from token counts.

    Per context and budget: P shared-prefix tokens (the same for every question about the repository),
    S question-specific context tokens and Q tokens of the template and the question after the context.
    The first question pays the input price on P + S + Q; each follow-up pays the cached-input price on P
    and the input price on S + Q; a session of SESSION questions pays one first question and SESSION - 1
    follow-ups. With --layouts (recall.py --layouts, on the development repositories), P, S and Q are the
    measured means; otherwise the nominal shares of the budget (the standard pack counted with no shared
    prefix)."""
    lay, lay_label = None, None
    if a.layouts:
        lay_label, sep, path = a.layouts.rpartition("=")
        with open(path if sep else a.layouts) as f:
            lay = json.load(f)
        lay_label = lay_label if sep else None
    dev = [r["name"] for r in cfg["repos"] if r["role"] == "development"]
    nq, qtok = estimate.load_questions(os.path.join(HERE, "questions"), a.texts, cfg)
    q_nominal = estimate.template_tokens() + sum(qtok[n] * nq[n] for n in dev) / max(1, sum(nq[n] for n in dev))

    def pooled(budget, arm):
        rows = [s for s in (lay or {}).get("rows", []) if s["scope"] == "all" and s["budget"] == budget
                and s["arm"] == arm and s["repo"] in dev]
        n = sum(s["n"] for s in rows)
        if not n:
            return None
        mean = lambda k: sum((s.get(k) or 0) * s["n"] for s in rows) / n
        if arm == "truncation":
            qs = pooled(budget, "pack")
            return {"n": n, "prefix": mean("tokens"), "suffix": 0.0, "question": qs["question"] if qs else q_nominal,
                    "recall": mean("recall")}
        return {"n": n, "prefix": mean("prefix_tokens"), "suffix": mean("suffix_tokens"),
                "question": mean("question_tokens"), "recall": mean("recall"),
                "prefix_shas": max(s.get("prefix_shas") or 0 for s in rows) if arm != "pack" else None}

    out_rows = []
    for budget in COST_BUDGETS:
        B = BUDGETS[budget]
        for arm, name, share in COST_CONTEXTS:
            m = pooled(budget, arm) if lay else None
            if lay and m is None:
                continue
            if m is None:
                prefix = share * B if share is not None else 0.0
                m = {"n": None, "prefix": prefix, "suffix": B - prefix, "question": q_nominal}
            row = {"budget": budget, "arm": arm, "context": name, "questions": m["n"],
                   "prefix_tokens": round(m["prefix"]), "suffix_tokens": round(m["suffix"]),
                   "question_tokens": round(m["question"]), "recall": m.get("recall"),
                   "prefix_hashes_per_repository": m.get("prefix_shas"), "models": {}}
            for model in COST_MODELS:
                p = prices["models"][model]
                first = (m["prefix"] + m["suffix"] + m["question"]) * p["input"] / 1e6
                follow = (m["prefix"] * p["cached_input"] + (m["suffix"] + m["question"]) * p["input"]) / 1e6
                row["models"][model] = {"first_usd": first, "follow_up_usd": follow,
                                        "session_mean_usd": (first + (SESSION - 1) * follow) / SESSION,
                                        "follow_up_vs_first": follow / first if first else None}
            out_rows.append(row)
    present = [n for n in dev if any(s["repo"] == n for s in (lay or {}).get("rows", []))]
    return {"label": COST_LABEL, "basis": "measured" if lay else "nominal", "pack_build": lay_label, "session": SESSION,
            "models": {m: {k: prices["models"][m][k] for k in ("input", "cached_input", "output")} for m in COST_MODELS},
            "prices_date": prices["date"], "repos": present if lay else None, "tokenizer": lay and lay.get("tokenizer"),
            "question_tokens_nominal": round(q_nominal), "rows": out_rows}


def by_role(d):
    """Each run's means pooled over the repositories of one role (weighted by questions)."""
    out = {}
    for role in ("held-out", "development"):
        repos = [r["name"] for r in d["repos"] if r["role"] == role]
        out[role] = {}
        for label, run in d["runs"].items():
            sel = [run["rows"][n]["all"] for n in repos if n in run["rows"]]
            n = sum(s["n"] for s in sel)
            if not n:
                continue
            row = {"n": n, "repos": [x for x in repos if x in run["rows"]], "arm": run["meta"]["arm"],
                   "budget": run["meta"]["budget"], "model": run["meta"]["model"]}
            for k in ("hit1", "hit3", "hit5", "allgold5", "rr", "context_recall", "tokens_in", "tokens_out",
                      "cost_usd", "wall_seconds", "turns"):
                vals = [(s[k], s["n"]) for s in sel if s.get(k) is not None]
                row[k] = sum(v * w for v, w in vals) / sum(w for _, w in vals) if vals else None
            out[role][label] = row
    return out


def recall_section(d, w):
    """Context recall without a model: one table per recall run (its label names the pack build)."""
    w("## Context recall without a model\n")
    runs = d.get("recall") or []
    if not runs:
        w("Not run yet: for every question and budget, whether the pack and truncation contain the gold files.\n")
        return
    w("For every question and budget, with no model involved: the share of the question's gold files that are "
      "inside the context, for the pack and for truncation filled to the same budget, with tokens "
      f"{counter_text(d['counter'])}. The pack holds small files whole and large ones as excerpts and as outlines "
      "of their symbols, and a gold file counts as inside when any of its text is in the pack; truncation holds "
      "whole files only. \"Some inside\" is the share of questions with at least one gold file inside. "
      "Each table is one run, named by the pack build it used.\n")
    order = [r["name"] for r in d["repos"]]
    roles = {r["name"]: r["role"] for r in d["repos"]}
    for rc in runs:
        rows = [s for s in rc["rows"] if s["scope"] == "all" and s["arm"] in ("pack", "truncation")]
        get = {(s["repo"], s["budget"], s["arm"]): s for s in rows}
        repos = sorted({s["repo"] for s in rows}, key=order.index)
        budgets = sorted({s["budget"] for s in rows}, key=BUDGETS.get)
        w(f"### {rc['label']}\n")
        w("| Repository | Role | Budget | Questions | Pack: gold files inside | Truncation: gold files inside | "
          "Pack: some inside | Truncation: some inside |")
        w("|---|---|---|---|---|---|---|---|")
        for role in ("held-out", "development"):
            names = [n for n in repos if roles.get(n) == role]
            groups = [(n, [n]) for n in names] + ([(f"all {role}", names)] if len(names) > 1 else [])
            for label, members in groups:
                for b in budgets:
                    cell = {}
                    for arm in ("pack", "truncation"):
                        sel = [get[(n, b, arm)] for n in members if (n, b, arm) in get]
                        k = sum(s["n"] for s in sel)
                        cell[arm] = {m: (sum(s[m] * s["n"] for s in sel) / k if k else None) for m in ("recall", "any")}
                        cell[arm]["n"] = k
                    name = f"**{label}**" if len(members) > 1 else label
                    w(f"| {name} | {role} | {b} | {cell['pack']['n'] or cell['truncation']['n']} | "
                      f"{pct(cell['pack']['recall'])} | {pct(cell['truncation']['recall'])} | {pct(cell['pack']['any'])} | "
                      f"{pct(cell['truncation']['any'])} |")
        w("")


def usd(v):
    return f"${v:.3f}" if v < 1 else f"${v:.2f}"


def cost_page_section(d, w):
    """The cost per question with and without the prompt cache, by context, budget and model."""
    c = d.get("cost")
    if not c:
        return
    names = {m: MODEL_NAMES.get(m, m) for m in c["models"]}
    w("## Cost per question with the prompt cache\n")
    w(f"**All numbers in this section are {c['label']}.** Runs with the model will replace them with billed costs.\n")
    prices = "; ".join(f"{names[m]} ${p['input']:.2f} per million input tokens, ${p['cached_input']:.2f} cached"
                       for m, p in c["models"].items())
    w("Meta's API serves the longest prefix a prompt shares with an earlier prompt from its cache, and bills it at "
      f"the cached-input price ({prices}; prices of {c['prices_date']}). A context whose first part is the same for "
      "every question about a repository pays the full input price once; every follow-up question pays the cached "
      "price on that part and the full price only on the rest. Truncation's dump is the same for every question, so "
      "it is cached the same way. These are input costs; output is billed on top "
      + ", ".join(f"(${p['output']:.2f} per million on {names[m]})" for m, p in c["models"].items()) + ".\n")
    if c["basis"] == "measured":
        build = f" ({c['pack_build']})" if c.get("pack_build") else ""
        w(f"The shared prefix and the question-specific tokens are the means over the questions of the development "
          f"repositories ({', '.join(c['repos'])}) for each context and budget{build}, counted with the Muse Spark Open "
          f"tokenizer. A first question pays for both, plus the question and the prompt template after the context.\n")
    else:
        w(f"No pack in the cache layout has been measured yet, so each context's shared prefix is its nominal share of "
          f"the budget (50% to 95% for the cache layout, all of it for the stable layout and for truncation); the "
          f"standard pack is counted with no shared prefix. The question and the prompt template add about "
          f"{c['question_tokens_nominal']:,} tokens.\n")
    w('<p align="center">\n  <picture>\n'
      '    <source media="(prefers-color-scheme: dark)" srcset="../assets/whole-repo-cost-dark.svg">\n'
      f'    <img src="../assets/whole-repo-cost-light.svg" alt="{cost_alt(c)}">\n  </picture>\n</p>\n')
    head = " | ".join(f"{names[m]}: first question | {names[m]}: each follow-up | {names[m]}: mean over "
                      f"{c['session']}" for m in c["models"])
    measured = c["basis"] == "measured"
    rec = " Gold files inside (development) |" if measured else ""
    w(f"| Budget | Context | Shared prefix (tokens) | Question-specific (tokens) |{rec} {head} |")
    w("|---|---|---|---|" + ("---|" if measured else "") + "---|" * (3 * len(c["models"])))
    for r in c["rows"]:
        if r["budget"] not in ("128K", "1M"):
            continue
        cells = " | ".join(f"{usd(x['first_usd'])} | {usd(x['follow_up_usd'])} | {usd(x['session_mean_usd'])}"
                           for x in r["models"].values())
        rc = f" {pct(r['recall'])} |" if measured else ""
        w(f"| {r['budget']} | {r['context']} | {r['prefix_tokens']:,} | {r['suffix_tokens'] + r['question_tokens']:,} "
          f"|{rc} {cells} |")
    if measured:
        same = all((r.get("prefix_hashes_per_repository") or 1) == 1 for r in c["rows"] if r["arm"] != "truncation")
        w(f"\nEvery question about one repository at one budget got {'the same' if same else 'NOT always the same'} "
          f"prefix bytes in the cache and stable layouts (one sha256 per repository, budget and layout"
          f"{'' if same else '; see the data file'}). Gold files inside: the share of each question's gold files with "
          f"any of their text in the context (in a pack, whole or as excerpts or outlines), the same measure as in "
          f"the context recall section.")
    w(f"\nThe 256K and 512K rows are in the data file [whole-repo.json](whole-repo.json). Whether a follow-up is served "
      f"from the cache is up to the API: the 24-hour retention is a hint, and a prompt that leaves the cache pays the "
      f"first-question price again.\n")


def cost_alt(c):
    """The cost chart's alt text: every bar's value."""
    names = {m: MODEL_NAMES.get(m, m) for m in c["models"]}
    parts = []
    for r in c["rows"]:
        if r["budget"] != "1M":
            continue
        parts.append(r["context"] + ": " + ", ".join(
            f"{names[m]} {usd(x['first_usd'])} without the cache, {usd(x['follow_up_usd'])} with it"
            for m, x in r["models"].items()))
    return (f"Input cost per follow-up question at a 1M-token budget, without and with the prompt cache, "
            f"{c['label']}. " + "; ".join(parts) + ".")


def registered_section(d, w):
    """The model runs as registered before any of them ran: grids, order, settings, tests, limits."""
    regs = d.get("registered") or []
    if not regs:
        return
    w("## Model runs, fixed before they ran\n")
    w("These runs, their order, their settings, the tests and the spending limits were fixed on 26 Oct 2026, before "
      "any model call. A grid runs only when the one before it has finished and only if it fits the money left; "
      "this page says which ones ran.\n")
    w("| Grid | Model | API | Arms, in run order | Questions | Estimated cost with caching (without) |")
    w("|---|---|---|---|---|---|")
    for g in regs:
        arms, qs = [], set()
        for it in g["items"]:
            t = GRID_ARM_TEXT.get(it["arm"], it["arm"]) + (f" at {it['budget']}" if it["budget"] else "")
            if it["first"]:
                t += f" (the first {it['first']} questions of each repository)"
            arms.append(t)
            qs.add(it["questions"])
        cap = f" of ${g['cap_usd']:.0f}" if g.get("cap_usd") else ""
        w(f"| {g['grid']} | {MODEL_NAMES.get(g['model'], g['model'])} (`{g['model']}`) | "
          f"{PROVIDER_NAMES.get(g['provider'], g['provider'])} | {', '.join(arms)} | "
          f"{max(qs)} held-out | ${g['estimate_cached_usd']:.2f} (${g['estimate_uncached_usd']:.2f}){cap} |")
    w("")
    w("The estimates fill every budget, count 4,000 output tokens per call and assume the search-only agent takes "
      "15 turns that add 2,500 tokens each (`estimate.py --registered`). The 1M arms on Muse Spark 1.3 (G2) and "
      "the Muse Wonder subset (G4) run only if the Meta limit still allows them after G1.\n")
    w("- **Caching:** Meta's API caches the longest identical prefix of a prompt and bills it at the cached-input "
      "price. Every call sends `prompt_cache_key`, one key per repository, snapshot, arm, budget and prefix hash, "
      "and `prompt_cache_retention: \"24h\"`; on Baseten the same key goes in the `x-session-affinity` header. "
      "Truncation, whose dump is the same for every question, gets the same treatment as the pack, so the cost "
      "comparison does not favour the pack.")
    w("- **Order:** the questions of one repository, arm and budget run back to back in the question file's order, "
      "the first one alone, so the later ones can be served from the cache. The search-only agent takes the three "
      "repositories in turn, one question each, so a run stopped by its spending limit still covers them evenly.")
    w("- **Settings:** reasoning effort medium; at most 8,192 output tokens per call (per turn for the agent); "
      "one answer per question; the API's default sampling.")
    w("- **Primary comparisons:** hit@5, pooled over the three held-out repositories, by McNemar's exact test: the "
      "pack against truncation at 128K and the pack against the search-only agent at 128K (Muse Spark 1.3), and the "
      "cache-layout pack against truncation at 1M (Muse Spark Open). Every other comparison is secondary. A question "
      "whose call still fails after retries is left out of the means and counted; the hit rates with such questions "
      "counted as misses are shown too.")
    w("- **Cost:** per question as billed, from the input, cached and output tokens the API reports, and at list "
      "prices without any cache.")
    w("- **Spending limits:** the runner logs every call (tokens, cached tokens, seconds, cost) and refuses to start "
      "a run when the logged total plus the run's own estimate would pass $45 on Meta's API or $100 on Baseten; "
      "within a run, no call is sent whose worst case (the whole prompt uncached plus the output limit) would pass "
      "the run's own limit or the total one.")
    w("- **Held-out repositories:** tuning happens only on kubernetes and home-assistant. Every run on pytorch, "
      "vscode or elasticsearch, with or without a model, is logged with its time and pack version before its result "
      "is read, and no held-out result changes the pack. The context recall runs once per release candidate; each "
      "hosted grid runs once, with one pack version, and a run stopped by an error or a spending limit is resumed, "
      "never repeated. The first held-out run is the context recall of the pack as it was built before 26 Oct 2026.\n")


def page(d):
    L = []
    w = L.append
    if d["test_data"]:
        w("> **TEST DATA.** This page was built from the mock server and the stand-in, to test the report "
          "generator. None of its result numbers measure a model or `graphicle pack`.\n")
    w("# Whole-repository change localization past 1M tokens\n")
    lo = min(r["tokens_all"] for r in d["repos"]) / 1e6
    hi = max(r["tokens_all"] for r in d["repos"]) / 1e6
    nq = sum(r["funnel"]["kept"] for r in d["repos"])
    w("Can a model find the files a real fix changed, in a repository far larger than its context window? "
      f"This benchmark asks it for five public repositories of {lo:.0f} to {hi:.0f} million tokens each, with {nq} questions taken "
      "from real issues and the files their fixes changed. It compares Graphicle's `graphicle pack` with plain "
      "truncation of the repository, with the same model searching the repository through tools, and with no "
      "repository context at all. The method was fixed before any result was seen, and the results are published "
      "as they came out.\n")
    w("## Results\n")
    if not d["runs"]:
        w("Not run yet. The questions, arms, prompts and scorer are fixed and committed; the model runs come next.\n")
    else:
        order = sorted(d["runs"], key=lambda l: ARM_ORDER.index(l) if l in ARM_ORDER else 99)
        for role in ("held-out", "development"):
            repos = [r["name"] for r in d["repos"] if r["role"] == role]
            labels = [l for l in order if l in d["by_role"].get(role, {})]
            if not labels:
                continue
            w(f"### {role.capitalize()} repositories ({', '.join(repos)})\n")
            w("| Arm | Questions | hit@1 | hit@3 | hit@5 [95% CI] | hit@5 without named paths | All gold in top 5 | "
              "MRR | Context recall | Tokens in per question | Tokens out per question | Cost per question | "
              "Seconds per question | Turns |")
            w("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|")
            for label in labels:
                r = d["by_role"][role][label]
                run = d["runs"][label]["rows"]
                c = run["pooled"]["all"].get("hit5_ci") if set(r["repos"]) == set(k for k in run if k != "pooled") else None
                wn = [run[n]["without_named"] for n in r["repos"]]
                nn = sum(x["n"] for x in wn)
                h5n = sum((x["hit5"] or 0) * x["n"] for x in wn) / nn if nn else None
                f3 = lambda v: "n/a" if v is None else f"{v:.3f}"
                f0 = lambda v: "n/a" if v is None else f"{v:,.0f}"
                f1 = lambda v: "n/a" if v is None else f"{v:.1f}"
                w(f"| {arm_name(label)} | {r['n']} | {pct(r['hit1'])} | {pct(r['hit3'])} | {pct(r['hit5'])}{ci(c)} | "
                  f"{pct(h5n)} ({nn}) | {pct(r['allgold5'])} | {f3(r['rr'])} | {pct(r['context_recall'])} | "
                  f"{f0(r['tokens_in'])} | {f0(r['tokens_out'])} | "
                  f"{'n/a' if r['cost_usd'] is None else '$%.3f' % r['cost_usd']} | {f1(r['wall_seconds'])} | "
                  f"{f1(r['turns'])} |")
            w("")
            w("| Repository | Arm | Questions | hit@1 | hit@5 [95% CI] | MRR | Context recall | Cost per question |")
            w("|---|---|---|---|---|---|---|---|")
            for n in repos:
                for label in labels:
                    rr = d["runs"][label]["rows"].get(n)
                    if not rr:
                        continue
                    x = rr["all"]
                    w(f"| {n} | {arm_name(label)} | {x['n']} | {pct(x['hit1'])} | {pct(x['hit5'])}{ci(x.get('hit5_ci'))} | "
                      f"{'n/a' if x['rr'] is None else '%.3f' % x['rr']} | {pct(x['context_recall'])} | "
                      f"{'n/a' if x['cost_usd'] is None else '$%.3f' % x['cost_usd']} |")
            w("")
        w("Questions with an API error are left out of the means and counted in the data file; the data file "
          "[whole-repo.json](whole-repo.json) also holds the intervals of every mean.\n")
        if d["paired"]:
            w("### Paired comparisons on hit@5 (McNemar's exact test, pooled over all repositories run)\n")
            w("| A | B | Questions | Only A hits | Only B hits | p |")
            w("|---|---|---|---|---|---|")
            for p in d["paired"]:
                if p["scope"] == "pooled":
                    w(f"| {arm_name(p['a'])} | {arm_name(p['b'])} | {p['n']} | {p['a_only']} | {p['b_only']} | "
                      f"{p['mcnemar_p']:.3g} |")
            w("")
    recall_section(d, w)
    cost_page_section(d, w)
    w("## Repositories\n")
    w(f"Each repository is pinned at S, the last commit on its default branch's first-parent line at or before "
      f"{d['snapshot_cutoff'].replace('T00:00:00Z', ' 00:00 UTC')}. Meta's model documentation states no knowledge "
      f"cutoff for Muse Spark, so the date was not moved. Sizes count the tracked text files at S, in tokens "
      f"{counter_text(d['counter'])}" + ("; in brackets, the estimate of UTF-8 bytes / 4" if d.get("estimate_counter")
                                          else "") + "; "
      f"vendored and generated files are listed in `bench/whole-repo/paths.py`.\n")
    w("| Repository | Role | S | S date (UTC) | Tokens, all | Without vendored and generated | Non-test source | "
      "Text files | Index time | Index size |")
    w("|---|---|---|---|---|---|---|---|---|---|")
    for r in d["repos"]:
        it = f"{r['index_seconds']:.0f} s" if r["index_seconds"] else "n/a"
        isz = f"{r['index_bytes'] / 2**30:.1f} GiB" if r["index_bytes"] else "n/a"
        e = r.get("estimate") or {}
        tk = lambda k: mtok(r[k]) + (f" ({mtok(e[k])})" if k in e else "")
        w(f"| {r['slug']} | {r['role']} | `{r['S'][:12]}` | {r['S_date'][:16].replace('T', ' ')} | {tk('tokens_all')} | "
          f"{tk('tokens_without_vendored_generated')} | {tk('tokens_nontest_source')} | {r['text_files']:,} | "
          f"{it} | {isz} |")
    g = next((r["graphicle"] for r in d["repos"] if r.get("graphicle")), "")
    w(f"\nIndex time is the wall time of `graphicle sync` ({g}) on 48 cores of a shared machine at low priority, "
      f"two repositories at a time; index size is its data folder.\n")
    w("## Questions\n")
    w(f"Each question is a real issue, fixed by a pull request merged into the default branch after S and by "
      f"{d['merged_until'][:10]}. The question is the issue's title and body as first filed (from GitHub's edit "
      f"history), with HTML comments and empty template headings removed. The gold answer is the source files the "
      f"pull request changed that exist at S: no tests, docs, changelogs, generated or vendored files. Only pull "
      f"requests with 1 to 5 gold files are kept, and only when their change to the gold files applies cleanly to S, "
      f"so the bug is present in the snapshot. When more questions pass than the target, the target number is drawn "
      f"with the fixed seed {d['selection_seed']}. The question sets publish the issue and pull request links, the "
      f"gold files and the sha256 of each question text; `fetch_texts.py` rebuilds the texts and checks them.\n")
    w("| Repository | Pull requests found | No readable linked issue | Over 100 files | Not 1 to 5 source files "
      "| Diff not readable | Not 1 to 5 gold files | Does not apply at S | Issue not usable | Issue already used | "
      "Passed | Kept | Text names a gold path |")
    w("|---|---|---|---|---|---|---|---|---|---|---|---|---|")
    for r in d["repos"]:
        f = r["funnel"]
        w(f"| {r['slug']} | {f['fetched']} | {f['dropped_linked']} | {f['dropped_files']} | {f['dropped_prefilter']} | "
          f"{f['dropped_diff']} | {f['dropped_gold']} | {f['dropped_applies']} | {f['dropped_issue']} | "
          f"{f['dropped_duplicate']} | {f['passed']} | {f['kept']} | {f['names_gold_path']} |")
    w("\nThe search found the merged pull requests GitHub links to an issue. For PyTorch, whose merge bot closes pull "
      "requests instead of merging them, every landed pull request was read and its closing keywords parsed, so most "
      "PyTorch pull requests drop out at the linked-issue step. Questions whose text names a gold file path (a "
      "path of at least two parts, such as a stack trace line) are flagged; the results are shown with and without "
      "them.\n")
    w("## Arms\n")
    w("Every arm sends one prompt: the context, then the question, in this template:\n")
    w("```text\n" + PROMPT.format(context="<context>\n\n", repo="<org>/<name>", sha="<S>", issue="<issue text>")
      + "\n```\n")
    w("- **Graphicle pack at B tokens:** `graphicle pack \"<issue>\" --project <repository> --budget B --format xml`.")
    w("- **Graphicle pack, cache layout:** the same with `--layout cache --prefix-share 0.9`: the first 90% of the "
      "budget is a prefix that does not depend on the question, byte for byte the same for every question about the "
      "repository at S, and the rest is chosen for the question. A model API's prompt cache can then serve the prefix "
      "of every question after the first at its cached-input price. With `--prefix-share 1.0` (the stable layout) "
      "the whole pack is the same for every question.")
    w("- **Truncation at B tokens:** the repository's files in a fixed order up to B: source files, then tests, then "
      "docs, each group by path, vendored and generated files left out, each file wrapped with its path like the "
      "pack's chunks; a file that would pass the budget is skipped.")
    w("- **Search-only agent:** the same model with four tools over the checkout at S: `grep` (at most 100 lines), "
      "`find` (at most 200 paths), `list_dir` and `read` (at most 400 lines per call), for at most 30 turns, then "
      "the same answer. Every token of every turn is counted.")
    w("- **No context:** the question alone, the control for memorisation.\n")
    w(f"Budgets B: {', '.join(f'{k} = {v:,}' for k, v in BUDGETS.items())} tokens (1M leaves 32,768 tokens of the "
      f"1,048,576-token window for the question and the answer). Pack and truncation are measured in the same "
      f"tokenizer ({counter_text(d['counter'])}).\n")
    w("## Metrics\n")
    w("- **hit@k:** a gold file is among the first k files of the answer (k = 1, 3, 5). **All gold in top 5:** every "
      "gold file is among the first 5. **MRR:** the mean of 1/rank of the first gold file (0 when none of the 10 is).")
    w("- **Context recall:** the share of gold files whose text is inside the context; for the agent, the share it read.")
    w("- **Tokens and cost:** from the API's usage fields; cost from the price table in `prices.json` "
      f"({d['prices']['date']}, {d['prices']['source'].split(' ')[0]}).")
    w("- **Tests:** McNemar's exact test on hit@5 between arms on the same questions; 95% bootstrap intervals "
      "(questions resampled) on the means.\n")
    registered_section(d, w)
    w("## Limitations\n")
    w(f"- Five repositories and {nq} questions: differences of a few points are within the noise; the intervals say how much.")
    w("- File-level localization only, scored by exact path; a fix can be correct in other files than the ones the "
      "pull request changed.")
    w("- The gold files come from what the fix changed, which is not always where the bug was reported.")
    w("- Issues filed before S can be in a model's training data; the no-context arm measures how much that helps.")
    w("- Pull requests whose changes do not apply at S were dropped, which favours fixes to code that did not move "
      "in the weeks after S.\n")
    w("## Reproduce\n")
    w("The scripts and question sets are in [`bench/whole-repo/`](../../bench/whole-repo/). In order: "
      "`checkout.py` (the snapshots), `fetch_texts.py` (the question texts, checked against their hashes), "
      "`index.py`, `tokens.py sizes`, `run.py` once per arm (with `--spend-cap`), `score.py`, `recall.py` (with "
      "`--layouts` for the cost section) and `report.py`; the charts come from `whole_repo_chart.py` and "
      "`whole_repo_cost_chart.py` in this folder. `estimate.py` prints a grid's tokens and cost before it runs.\n")
    text = "\n".join(L)
    for bad in ("—", "–"):
        if bad in text:
            raise SystemExit("the page holds a dash it must not")
    return text


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sizes", required=True)
    ap.add_argument("--sizes-estimate", help="sizes counted by the estimate, shown in brackets")
    ap.add_argument("--counter-label", help="plain words for the tokenizer, for the page")
    ap.add_argument("--index-dir", required=True)
    ap.add_argument("--score")
    ap.add_argument("--recall", action="append",
                    help="LABEL=RECALL.json (recall.py's summary; LABEL names the pack build), once per run")
    ap.add_argument("--texts", help="the question texts (fetch_texts.py), for the issue tokens in the estimates")
    ap.add_argument("--layouts", help="[LABEL=]recall.py --layouts summary (development repositories) for the cost "
                                      "section; LABEL names the pack build")
    ap.add_argument("--json", required=True)
    ap.add_argument("--md", required=True)
    ap.add_argument("--test", action="store_true")
    a = ap.parse_args()
    d = build(a)
    with open(a.json, "w") as f:
        json.dump(d, f, indent=1, sort_keys=True)
    with open(a.md, "w") as f:
        f.write(page(d))
    print(f"wrote {a.json} and {a.md}")


if __name__ == "__main__":
    main()
