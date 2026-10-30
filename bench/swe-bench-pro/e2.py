#!/usr/bin/env python3
"""E2: Muse Spark's answers on the SWE-bench Pro sample, with no context, BM25 at 64K or the pack at 64K.

usage: e2.py --work WORK --arm noctx|bm25|pack --out OUT [--ids sample-e2.json] [--profile meta|mock]
             [--base-url URL] [--model muse-spark-1.3] [--key-file FILE] [--spend-cap auto|DOLLARS]
             [--spend-log FILE] [--caps caps.json] [--prices prices.json] [--jobs 4] [--limit N]
             [--tokenizer FILE] [--reasoning-effort medium] [--max-output-tokens 8192]

The contexts are the files e1.py saved for the sample (WORK/contexts/<id>/pack-64K.xml and bm25-64K.txt),
checked against the sha256 that E1 recorded. The prompt is the whole-repository benchmark's (whole-repo-v1),
with the task's repository, base commit and problem statement; the answer is scored with its scorer against
the task's gold files, paths normalised against the files at the base commit. Every call goes through the
spend log and caps of ../whole-repo/run.py: the run refuses to start when the provider's logged total plus its
estimate would pass the provider's cap (caps.json), and a call is not sent when its worst case would pass the
run's cap or what is left of the provider's. Writes OUT/run.json, OUT/rows.jsonl (score.py reads them) and
OUT/raw/ (responses, not for publishing). Tasks already in rows.jsonl are skipped; a failed call is tried once
more at the end of the run.
"""
import argparse
import fcntl
import hashlib
import json
import math
import os
import subprocess
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor

HERE = os.path.dirname(os.path.abspath(__file__))
WR = os.path.join(HERE, "..", "whole-repo")
sys.path.insert(0, WR)
from run import (CACHE_RETENTION, PROMPT, PROMPT_VERSION, PROFILES, CapReached, Client, Spend, logged_total,  # noqa: E402
                 now_iso, parse_files, text_of, usage_of, user_message)
from score import normalize_answer, score_answer  # noqa: E402
from tokens import Counter  # noqa: E402

sys.path.insert(0, HERE)
from e1 import run_paths  # noqa: E402

CONTEXT_FILES = {"pack": "pack-64K.xml", "bm25": "bm25-64K.txt"}
BUDGET = "64K"
OUT_TOKENS_ESTIMATE = 4000


def load(work, ids_file, limit):
    tasks = {t["instance_id"]: t for t in map(json.loads, open(os.path.join(work, "tasks.jsonl")))}
    d = json.load(open(ids_file))
    ids = d["ids"] if isinstance(d, dict) else d
    return [tasks[i] for i in (ids[:limit] if limit else ids)]


def context_of(work, arm, iid, run="e1"):
    """(text, sha256, tokens recorded by E1) of a task's saved context from E1's run `run`; '' for noctx."""
    if arm == "noctx":
        return "", hashlib.sha256(PROMPT.encode()).hexdigest(), 0
    _scratch, outdir, ctx_root, _logs = run_paths(work, run)
    info = json.load(open(os.path.join(outdir, "tasks", iid + ".json")))
    rec = info["contexts"][CONTEXT_FILES[arm]]
    text = open(os.path.join(ctx_root, iid, CONTEXT_FILES[arm]), encoding="utf-8").read()
    sha = hashlib.sha256(text.encode("utf-8")).hexdigest()
    if sha != rec["sha256"]:
        raise SystemExit(f"{iid}: the saved {arm} context does not match E1's sha256")
    return text, sha, rec["tokens"]


def files_at(work, t):
    mirror = os.path.join(work, "mirror", t["repo"].replace("/", "__") + ".git")
    out = subprocess.run(["git", "-C", mirror, "ls-tree", "-r", "--name-only", "-z", t["base_commit"]],
                         capture_output=True, text=True, check=True).stdout
    return {p for p in out.split("\0") if p}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True)
    ap.add_argument("--arm", required=True, choices=["noctx", "bm25", "pack"])
    ap.add_argument("--out", required=True)
    ap.add_argument("--ids", default=os.path.join(HERE, "sample-e2.json"))
    ap.add_argument("--e1-run", default="e1", help="the E1 run whose saved contexts are used")
    ap.add_argument("--profile", default="meta", choices=["meta", "mock"])
    ap.add_argument("--base-url")
    ap.add_argument("--model")
    ap.add_argument("--key-file")
    ap.add_argument("--spend-cap", default="auto")
    ap.add_argument("--spend-log")
    ap.add_argument("--caps", default=os.path.join(WR, "caps.json"))
    ap.add_argument("--prices", default=os.path.join(WR, "prices.json"))
    ap.add_argument("--jobs", type=int, default=4)
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--tokenizer")
    ap.add_argument("--reasoning-effort", default="medium")
    ap.add_argument("--max-output-tokens", type=int, default=8192)
    a = ap.parse_args()
    prof = PROFILES[a.profile]
    base_url = a.base_url or prof["base_url"]
    model = a.model or prof["model"]
    if not base_url:
        raise SystemExit(f"--base-url is needed with --profile {a.profile}")
    work = os.path.abspath(a.work)
    spend_log = os.path.abspath(a.spend_log or os.path.join(work, "spend", "spend-log.jsonl"))
    prices = json.load(open(a.prices))
    if model not in prices["models"]:
        raise SystemExit(f"no price for {model} in {a.prices}")
    price = prices["models"][model]
    provider = price.get("provider", a.profile)
    caps = json.load(open(a.caps))["usd"] if os.path.exists(a.caps) else {}
    global_cap = caps.get(provider)
    key = open(a.key_file).read().strip() if a.key_file else os.environ.get(prof["key_env"], "")
    if not key:
        raise SystemExit(f"no key: set {prof['key_env']} or pass --key-file")
    os.makedirs(os.path.join(a.out, "raw"), exist_ok=True)
    os.makedirs(os.path.dirname(spend_log), exist_ok=True)
    lock = open(spend_log + f".{provider}.lock", "w")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        sys.exit(f"REFUSED: another run for {provider} holds {lock.name}")

    counter = Counter(a.tokenizer)
    label = a.arm if a.arm == "noctx" else f"{a.arm}@{BUDGET}"
    tasks = load(work, a.ids, a.limit)
    rows_path = os.path.join(a.out, "rows.jsonl")
    done = {json.loads(l)["id"] for l in open(rows_path)} if os.path.exists(rows_path) else set()
    todo, left_out = [], []
    for t in tasks:
        if t["instance_id"] in done:
            continue
        if not os.path.exists(os.path.join(run_paths(work, a.e1_run)[1], "tasks", t["instance_id"] + ".json")):
            # E1 failed on it twice: such a task is left out of every arm, noctx included.
            left_out.append(t["instance_id"])
            continue
        text = open(os.path.join(work, "texts", t["instance_id"] + ".txt"), encoding="utf-8").read()
        ctx, sha, ctx_tokens = context_of(work, a.arm, t["instance_id"], a.e1_run)
        prompt = PROMPT.format(context=ctx + "\n\n" if ctx else "", repo=t["repo"], sha=t["base_commit"], issue=text)
        todo.append((t, prompt, sha, ctx_tokens))

    def cost(tin, tcached, tout):
        return ((tin - tcached) * price["input"] + tcached * price.get("cached_input", price["input"])
                + tout * price["output"]) / 1e6

    est = sum(cost(counter.count(p), 0, OUT_TOKENS_ESTIMATE) for _t, p, _s, _n in todo)
    logged = logged_total(spend_log, provider)
    print(f"{label}: {len(todo)} of {len(tasks)} tasks to do ({len(left_out)} left out: E1 failed on them); "
          f"estimate ${est:.2f} (no cache reuse across tasks); {provider} log total ${logged:.2f}"
          + (f", cap ${global_cap:.2f}" if global_cap is not None else ""), flush=True)
    if global_cap is not None and logged + est > global_cap:
        sys.exit(f"REFUSED: the {provider} log total ${logged:.2f} plus this run's estimate ${est:.2f} would pass "
                 f"the cap ${global_cap:.2f}")
    spent_before = json.load(open(os.path.join(a.out, "spend.json")))["spent_usd"] \
        if os.path.exists(os.path.join(a.out, "spend.json")) else 0.0
    if str(a.spend_cap).lower() == "auto":
        cap = spent_before + math.ceil(1.5 * est)
        if global_cap is not None:
            cap = min(cap, spent_before + global_cap - logged)
    else:
        cap = float(a.spend_cap)
    spend = Spend(os.path.join(a.out, "spend.json"), cap, price, spend_log, provider, global_cap)
    client = Client(base_url, key, "responses")
    meta = {"label": label, "arm": a.arm, "budget": None if a.arm == "noctx" else BUDGET,
            "budget_tokens": None if a.arm == "noctx" else 65536, "model": model, "api": "responses",
            "profile": a.profile, "provider": provider, "base_url": base_url, "reasoning_effort": a.reasoning_effort,
            "max_output_tokens": a.max_output_tokens, "prompt_version": PROMPT_VERSION, "counter": counter.name,
            "context_source": f"E1 run {a.e1_run}'s saved contexts (graphicle pack, bm25.py)" if a.arm != "noctx"
            else "noctx", "e1_run": a.e1_run,
            "cache_retention": CACHE_RETENTION, "spend_cap_usd": cap, "global_cap_usd": global_cap,
            "spend_log": spend_log, "estimate_usd": est, "benchmark": "swe-bench-pro e2", "ids": a.ids,
            "left_out_e1_failed": left_out, "started": now_iso()}
    with open(os.path.join(a.out, "run.json"), "w") as f:
        json.dump(meta, f, indent=1, sort_keys=True)
    write_lock, stop = threading.Lock(), threading.Event()

    def one(item):
        t, prompt, sha, ctx_tokens = item
        iid = t["instance_id"]
        key_ = f"swepro:{iid}:{label}:{sha[:12]}"
        row = {"id": iid, "repo": t["repo"], "arm": a.arm, "label": label, "model": model, "gold": t["gold"],
               "names_gold_path": t["names_gold_path"], "cache_key": key_, "context_sha256": sha,
               "context_tokens": ctx_tokens, "error": None, "turns": 1}
        t0 = time.time()
        try:
            body = {"model": model, "input": [user_message(prompt)], "store": False,
                    "reasoning": {"effort": a.reasoning_effort}, "max_output_tokens": a.max_output_tokens,
                    "prompt_cache_key": key_, "prompt_cache_retention": CACHE_RETENTION}
            worst = spend.reserve(counter.count(prompt), a.max_output_tokens)
            rec = {"time": now_iso(), "provider": provider, "model": model, "api": "responses", "arm": label,
                   "budget": meta["budget"], "run": os.path.abspath(a.out), "cache_key": key_, "repo": t["repo"],
                   "question": iid, "turn": 1, "benchmark": "swe-bench-pro e2"}
            s0 = time.time()
            try:
                resp = client.post("/responses", body)
            except Exception as e:
                spend.settle(worst, 0.0, dict(rec, status="error", error=str(e)[:200], input_tokens=0, cached_tokens=0,
                                              output_tokens=0, reasoning_tokens=0, seconds=round(time.time() - s0, 3),
                                              cost_usd=0.0))
                raise
            tin, tcached, tout, treason = usage_of(resp, "responses")
            c = cost(tin, tcached, tout)
            spend.settle(worst, c, dict(rec, status="ok", input_tokens=tin, cached_tokens=tcached, output_tokens=tout,
                                        reasoning_tokens=treason, seconds=round(time.time() - s0, 3), cost_usd=c))
            answer = text_of(resp, "responses")
            files = parse_files(answer)
            norm = normalize_answer(files, files_at(work, t)) if files is not None else []
            row.update(score_answer(norm, t["gold"]))
            row.update({"files": norm, "answer_parsed": files is not None, "tokens_in": tin, "tokens_cached": tcached,
                        "tokens_out": tout, "tokens_reasoning": treason, "cost_usd": c,
                        "prompt_tokens_counted": counter.count(prompt)})
            with open(os.path.join(a.out, "raw", iid + ".json"), "w") as f:
                json.dump({"response": resp, "answer": answer}, f)
        except CapReached as e:
            stop.set()
            print(f"STOPPED: {e}", flush=True)
            return None
        except Exception as e:
            row["error"] = f"{type(e).__name__}: {str(e)[:300]}"
        row["wall_seconds"] = round(time.time() - t0, 2)
        return row

    def run(items):
        failed = []
        if not items:
            return failed

        def keep(row):
            if row is None:
                return
            if row["error"]:
                failed.append(row)
                return
            with write_lock, open(rows_path, "a") as f:
                f.write(json.dumps(row, sort_keys=True) + "\n")
            print(f"{row['id']}: hit@5 {row['hit5']}, ${row['cost_usd']:.4f}", flush=True)

        keep(one(items[0]))  # the first call alone
        with ThreadPoolExecutor(a.jobs) as ex:
            for row in ex.map(lambda it: None if stop.is_set() else one(it), items[1:]):
                keep(row)
        return failed

    failed = run(todo)
    if failed and not stop.is_set():
        again = [it for it in todo if it[0]["instance_id"] in {r["id"] for r in failed}]
        print(f"retrying {len(again)} failed tasks once", flush=True)
        failed = run(again)
        with open(rows_path, "a") as f:
            for row in failed:
                f.write(json.dumps(row, sort_keys=True) + "\n")
    meta.update({"finished": now_iso(), "spend_usd": spend.spent, "stopped_by_cap": stop.is_set()})
    with open(os.path.join(a.out, "run.json"), "w") as f:
        json.dump(meta, f, indent=1, sort_keys=True)
    print(f"{label}: done, ${spend.spent:.2f} spent" + (" (stopped by a cap)" if stop.is_set() else ""), flush=True)


if __name__ == "__main__":
    main()
