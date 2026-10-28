#!/usr/bin/env python3
"""Tokens and cost of a planned grid, printed before any run.

usage: estimate.py --questions DIR [--texts DIR] [--grid GRID] [--model muse-spark-1.3] [--registered]
                   [--prices prices.json] [--caps caps.json] [--out-tokens 4000] [--agent-turns 15]
                   [--agent-turn-tokens 2500] [--agent-out-per-turn 800] [--agent-rows ROWS.jsonl ...]
                   [--standard-prefix 0]

GRID is a comma-separated list of ARM[@BUDGET]:REPOS[:N]:
  ARM    noctx, pack, pack-cache (prefix share 0.9), pack-cacheF (prefix share F, such as
         pack-cache0.95), pack-stable (prefix share 1.0), trunc or agent
  REPOS  a '+'-joined list of names, or held-out, development or all
  N      the first N questions of each repository, in file order (default: all of them)
The default grid is the registered grid G1 (REGISTERED). --registered prints every registered grid,
each priced with its own model, and adds them up in their order against their provider's global cap
(caps.json: Meta $45, Baseten $100).

Per call, a pack or truncation arm sends its whole budget (an upper bound) plus the prompt and the
issue; no context sends the prompt and the issue; every call returns --out-tokens (reasoning
included). The search-only agent: each turn sends everything so far, so a question of T turns sends
T x (prompt + issue) + turn_tokens x T(T+1)/2 tokens in and T x out_per_turn out; --agent-rows
replaces these assumptions with the means of measured agent rows (run.py's rows.jsonl).

Caching. The runner sends the questions of one (repository, arm, budget) group back to back, the
first one alone, under one prompt_cache_key, so every later question can read the group's shared
prefix from the cache:
  trunc          the whole dump (the same text for every question)
  pack-stable    the whole pack (only the question after it is new)
  pack-cache[F]  F x B, the pack's question-independent prefix
  pack           --standard-prefix tokens (the standard layout's question-independent head; 0 here)
  noctx          nothing
  agent          within one question, each turn's earlier turns
The "cached" columns bill the first question of a group at the input price and every later one at
the cached-input price on the shared prefix; "uncached" is the worst case, with no cache hit at all.
Whether a call is served from the cache is up to the API, so both are shown.
Issue tokens are UTF-8 bytes / 4 when --texts is given, else 1,000 per question.
"""
import argparse
import json
import math
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from run import BUDGETS, PROMPT  # noqa: E402

# The pre-registered hosted grids (bench-results PREREG-2026-10-26.md), in their priority order.
REGISTERED = [
    ("G1", "Meta, grid C: the 128K arms on the 90 held-out questions", "muse-spark-1.3",
     "noctx:held-out,trunc@128K:held-out,pack@128K:held-out,agent:held-out"),
    ("G2", "Meta, the cache-aware pack at 1M on the first 10 held-out questions of each repository",
     "muse-spark-1.3", "pack-cache@1M:held-out:10"),
    ("G3", "Baseten, Muse Spark Open: the 1M answer arms on the held-out questions",
     "meta-models/muse-spark-open",
     "noctx:held-out,trunc@1M:held-out,pack-cache@1M:held-out,pack@1M:held-out:10"),
    ("G4", "Meta, a Muse Wonder subset, only if money is left", "muse-wonder-1.0", "pack-cache@128K:held-out:10"),
]
DEFAULT_GRID = REGISTERED[0][3]
ARM_RX = re.compile(r"pack-cache(?P<share>0?\.\d+|1(?:\.0+)?)?")


def arm_parts(arm):
    """(kind, prefix share): ("pack", None) for the standard pack, ("pack", 0.9) for pack-cache, ..."""
    m = ARM_RX.fullmatch(arm)
    if m:
        return "pack", float(m.group("share")) if m.group("share") else 0.9
    if arm == "pack-stable":
        return "pack", 1.0
    if arm in ("pack", "trunc", "noctx", "agent"):
        return arm, None
    raise SystemExit(f"unknown arm {arm!r}")


def shared_prefix(arm, budget_tokens, standard_prefix=0):
    """Tokens of the context that every question of a group sends unchanged."""
    kind, share = arm_parts(arm)
    if kind == "trunc":
        return budget_tokens
    if kind == "pack":
        return share * budget_tokens if share is not None else standard_prefix
    return 0


def cost_of(price, tin, tcached, tout):
    return ((tin - tcached) * price["input"] + tcached * price.get("cached_input", price["input"])
            + tout * price["output"]) / 1e6


def group_tokens(arm, budget, q, base, out_tokens, agent, standard_prefix=0):
    """(calls, tokens in, tokens cached when the cache works, tokens out) of one group of q questions.
    base is the prompt template plus the mean issue; agent holds turns, turn_tokens, out_per_turn and,
    when measured, tokens_in, tokens_cached and tokens_out per question."""
    if q <= 0:
        return 0, 0, 0, 0
    kind, _share = arm_parts(arm)
    if kind == "agent":
        if agent.get("measured"):
            m = agent["measured"]
            return q * m["turns"], q * m["tokens_in"], q * m["tokens_cached"], q * m["tokens_out"]
        t, tt = agent["turns"], agent["turn_tokens"]
        qin = t * base + tt * t * (t + 1) / 2
        return q * t, q * qin, q * (qin - base - tt * t), q * t * agent["out_per_turn"]
    if kind == "noctx":
        return q, q * base, 0, q * out_tokens
    b = BUDGETS[budget]
    return q, q * (b + base), (q - 1) * shared_prefix(arm, b, standard_prefix), q * out_tokens


def load_questions(questions, texts, cfg):
    nq, qtok = {}, {}
    for r in cfg["repos"]:
        p = os.path.join(questions, f"questions-{r['name']}.jsonl")
        nq[r["name"]] = sum(1 for l in open(p) if l.strip()) if os.path.exists(p) else 0
        toks = []
        if texts and os.path.exists(os.path.join(texts, f"texts-{r['name']}.jsonl")):
            toks = [math.ceil(len(json.loads(l)["text"].encode()) / 4)
                    for l in open(os.path.join(texts, f"texts-{r['name']}.jsonl"))]
        qtok[r["name"]] = (sum(toks) / len(toks)) if toks else 1000.0
    return nq, qtok


def template_tokens():
    return math.ceil(len(PROMPT.encode()) / 4) + 30


def repo_names(spec, roles):
    if spec in ("held-out", "development"):
        return [n for n, r in roles.items() if r == spec]
    if spec == "all":
        return list(roles)
    return spec.split("+")


def grid_rows(grid, nq, qtok, roles, price, out_tokens, agent, standard_prefix=0):
    """One row per grid item: label, repos, calls, tokens in, cached, out, cost uncached, cost cached."""
    rows = []
    for item in grid.split(","):
        parts = item.split(":")
        arm_b, spec = parts[0], parts[1]
        limit = int(parts[2]) if len(parts) > 2 else 0
        arm, _, budget = arm_b.partition("@")
        names = repo_names(spec, roles)
        calls = tin = tcached = tout = 0.0
        for n in names:
            q = min(nq[n], limit) if limit else nq[n]
            c, i, ca, o = group_tokens(arm, budget, q, template_tokens() + qtok[n], out_tokens, agent,
                                       standard_prefix)
            calls, tin, tcached, tout = calls + c, tin + i, tcached + ca, tout + o
        rows.append({"item": arm_b + (f" (first {limit})" if limit else ""), "repos": "+".join(names),
                     "calls": calls, "tokens_in": tin, "tokens_cached": tcached, "tokens_out": tout,
                     "uncached": cost_of(price, tin, 0, tout), "cached": cost_of(price, tin, tcached, tout)})
    return rows


def run_estimate(a_questions, a_texts, arm, budget, repos, limit, model, prices, out_tokens=4000, agent=None,
                 standard_prefix=0):
    """The cost of one run.py run (cached and uncached), for its refusal to start past the global cap."""
    cfg = json.load(open(os.path.join(HERE, "repos.json")))
    nq, qtok = load_questions(a_questions, a_texts, cfg)
    names = repos or [r["name"] for r in cfg["repos"] if nq.get(r["name"])]
    agent = agent or {"turns": 15, "turn_tokens": 2500, "out_per_turn": 800}
    tot = {"uncached": 0.0, "cached": 0.0}
    for n in names:
        q = min(nq[n], limit) if limit else nq[n]
        c, i, ca, o = group_tokens(arm, budget, q, template_tokens() + qtok[n], out_tokens, agent, standard_prefix)
        tot["uncached"] += cost_of(prices["models"][model], i, 0, o)
        tot["cached"] += cost_of(prices["models"][model], i, ca, o)
    return tot


def print_rows(rows):
    print(f"{'arm':22s} {'repos':28s} {'calls':>6s} {'tokens in':>13s} {'cached in':>13s} {'tokens out':>11s} "
          f"{'uncached':>9s} {'cached':>8s}")
    for r in rows:
        print(f"{r['item']:22s} {r['repos']:28s} {r['calls']:6.0f} {r['tokens_in']:13,.0f} {r['tokens_cached']:13,.0f} "
              f"{r['tokens_out']:11,.0f} {r['uncached']:9.2f} {r['cached']:8.2f}")
    tu, tc = sum(r["uncached"] for r in rows), sum(r["cached"] for r in rows)
    print(f"{'total':22s} {'':28s} {'':6s} {'':13s} {'':13s} {'':11s} {tu:9.2f} {tc:8.2f}")
    return tu, tc


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--questions", required=True)
    ap.add_argument("--texts")
    ap.add_argument("--grid", default=DEFAULT_GRID)
    ap.add_argument("--model", default="muse-spark-1.3")
    ap.add_argument("--registered", action="store_true", help="every registered grid against the global caps")
    ap.add_argument("--prices", default=os.path.join(HERE, "prices.json"))
    ap.add_argument("--caps", default=os.path.join(HERE, "caps.json"))
    ap.add_argument("--out-tokens", type=int, default=4000)
    ap.add_argument("--agent-turns", type=float, default=15)
    ap.add_argument("--agent-turn-tokens", type=float, default=2500)
    ap.add_argument("--agent-out-per-turn", type=float, default=800)
    ap.add_argument("--agent-rows", nargs="*")
    ap.add_argument("--standard-prefix", type=float, default=0,
                    help="question-independent tokens at the head of a standard pack")
    a = ap.parse_args()
    cfg = json.load(open(os.path.join(HERE, "repos.json")))
    roles = {r["name"]: r["role"] for r in cfg["repos"]}
    prices = json.load(open(a.prices))
    caps = json.load(open(a.caps))["usd"]
    nq, qtok = load_questions(a.questions, a.texts, cfg)
    agent = {"turns": a.agent_turns, "turn_tokens": a.agent_turn_tokens, "out_per_turn": a.agent_out_per_turn}
    if a.agent_rows:
        rows = [json.loads(l) for p in a.agent_rows for l in open(p) if l.strip()]
        rows = [r for r in rows if not r.get("error")]
        if rows:
            agent["measured"] = {k: sum(r[k] for r in rows) / len(rows)
                                 for k in ("turns", "tokens_in", "tokens_cached", "tokens_out")}
            agent["measured"]["n"] = len(rows)
    print(f"prices: prices.json ({prices['date']}); questions: "
          + ", ".join(f"{k} {v} (issue ~{qtok[k]:,.0f} tokens)" for k, v in nq.items()))
    m = agent.get("measured")
    print(f"agent: measured means over {m['n']} rows: {m['turns']:.1f} turns, {m['tokens_in']:,.0f} in "
          f"({m['tokens_cached']:,.0f} cached), {m['tokens_out']:,.0f} out" if m else
          f"agent: assumed {a.agent_turns:g} turns, {a.agent_turn_tokens:,.0f} tokens added per turn, "
          f"{a.agent_out_per_turn:,.0f} out per turn")
    print(f"every call: {a.out_tokens:,} tokens out (reasoning included); budgets are upper bounds")
    grids = ([(g, d, model, grid) for g, d, model, grid in REGISTERED] if a.registered
             else [("grid", "", a.model, a.grid)])
    spent = {}
    for name, desc, model, grid in grids:
        p = prices["models"][model]
        prov = p.get("provider", "?")
        print(f"\n== {name}: {desc}" if desc else f"\n== {name}")
        print(f"model {model} ({prov}): ${p['input']}/M in, ${p.get('cached_input', p['input'])}/M cached in, "
              f"${p['output']}/M out")
        tu, tc = print_rows(grid_rows(grid, nq, qtok, roles, p, a.out_tokens, agent, a.standard_prefix))
        if prov in caps:
            before = spent.get(prov, (0.0, 0.0))
            spent[prov] = (before[0] + tu, before[1] + tc)
            cap = caps[prov]
            print(f"{prov} cap ${cap:.0f}: this grid ${tc:.2f} cached (${tu:.2f} uncached); with the grids before it "
                  f"${spent[prov][1]:.2f} cached ({'fits' if spent[prov][1] <= cap else 'OVER'}), "
                  f"${spent[prov][0]:.2f} uncached ({'fits' if spent[prov][0] <= cap else 'OVER'})")
    if a.registered:
        print("\nglobal caps (caps.json): " + ", ".join(f"{k} ${v:.0f}" for k, v in caps.items()))
        for prov, (tu, tc) in spent.items():
            print(f"{prov}: all registered grids ${tc:.2f} cached, ${tu:.2f} uncached, cap ${caps[prov]:.0f}")


if __name__ == "__main__":
    main()
