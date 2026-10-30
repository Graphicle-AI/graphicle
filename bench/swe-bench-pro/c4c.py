#!/usr/bin/env python3
"""C4c: the billed cache meter. One 1M-token kubernetes pack, then Muse Spark and Muse Wonder follow-ups.

usage:
  c4c.py prepare --work WORK --graphicle BIN --tokenizer TOKENIZER.json --texts TEXTS [--budget 1015808]
      checks kubernetes out at its snapshot S (WORK/c4c/src/kubernetes, one shallow fetch), builds its index with
      BIN (WORK/c4c/data/kubernetes, timed), then the pack of each of the first 20 kubernetes questions:
      `graphicle pack "<issue>" --budget B --layout cache --prefix-share 0.9 --format xml|json --tokenizer ...`.
      Every pack must have the same prefix_sha256; writes WORK/c4c/packs/<id>.xml and WORK/c4c/packs.json.
  c4c.py run --work WORK --out OUT [--profile meta|mock] [--base-url URL] [--key-file FILE] [--spend-cap auto]
             [--spend-log FILE] [--caps caps.json] [--prices prices.json] [--tokenizer FILE] [--skip-count]
      the session: muse-spark-1.3 on questions 1-20, then muse-wonder-1.0 on questions 1-10, one call at a time
      in question order, each model's first call alone, all under one prompt_cache_key with 24-hour retention.
      Before the first billed call, POST /responses/input_tokens counts call 1's prompt; over 1,040,384 tokens
      the run stops and asks for packs built at 983,040 (prepare --budget 983040). If Spark's second call is
      served less than half from the cache, the session stops after Spark's third call. Writes OUT/calls.jsonl
      (tokens as Meta reports them, the cached share and the dollars those usage fields bill at prices.json),
      OUT/summary.json (the first call against the follow-ups, per model) and OUT/raw/.

The questions are the whole-repository benchmark's kubernetes questions (a development repository), in file
order; their answers are scored as a secondary. Every call goes through the spend log and caps of
../whole-repo/run.py.
"""
import argparse
import fcntl
import hashlib
import json
import math
import os
import statistics
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
WR = os.path.join(HERE, "..", "whole-repo")
sys.path.insert(0, WR)
from pack_arm import pack_command  # noqa: E402
from run import (CACHE_RETENTION, PROMPT, PROFILES, CapReached, Client, Spend, logged_total, now_iso,  # noqa: E402
                 parse_files, text_of, usage_of, user_message)
from score import normalize_answer, score_answer  # noqa: E402
from tokens import Counter  # noqa: E402

N_SPARK, N_WONDER = 20, 10
SPARK, WONDER = "muse-spark-1.3", "muse-wonder-1.0"
BUDGET, SMALLER_BUDGET, SHARE = 1015808, 983040, 0.9
WINDOW, OUT_CAP = 1048576, 8192
OUT_TOKENS_ESTIMATE = 4000


QUESTIONS = os.path.join(WR, "questions", "questions-kubernetes.jsonl")


def questions(texts, path=QUESTIONS):
    qs = [json.loads(l) for l in open(path) if l.strip()]
    tx = {t["id"]: t["text"] for t in map(json.loads, open(os.path.join(texts, "texts-kubernetes.jsonl")))}
    for q in qs:
        if hashlib.sha256(tx[q["id"]].encode("utf-8")).hexdigest() != q["text_sha256"]:
            raise SystemExit(f"{q['id']}: the text does not match its text_sha256")
    return [(q, tx[q["id"]]) for q in qs[:N_SPARK]]


def cmd_prepare(a):
    work = os.path.abspath(a.work)
    snap = json.load(open(os.path.join(WR, "snapshots.json")))["kubernetes"]["sha"]
    src = os.path.abspath(a.src or os.path.join(work, "c4c", "src", "kubernetes"))
    data = os.path.abspath(a.data or os.path.join(work, "c4c", "data", "kubernetes"))
    if a.data and os.path.isdir(a.data) and os.listdir(a.data):
        # An index the same binary built already (node_setup.sh g1): recorded, not rebuilt.
        os.makedirs(os.path.join(work, "c4c"), exist_ok=True)
        with open(os.path.join(work, "c4c", "index.json"), "w") as f:
            json.dump({"graphicle": subprocess.run([a.graphicle, "--version"], capture_output=True,
                                                   text=True).stdout.strip(), "data": data, "reused": True}, f)
    if not os.path.exists(os.path.join(src, ".git")):
        os.makedirs(src, exist_ok=True)
        for cmd in (["git", "init", "-q"], ["git", "remote", "add", "origin", "https://github.com/kubernetes/kubernetes.git"],
                    ["git", "fetch", "-q", "--depth", "1", "origin", snap], ["git", "checkout", "-q", "--detach", "FETCH_HEAD"]):
            subprocess.run(cmd, cwd=src, check=True)
    head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=src, capture_output=True, text=True).stdout.strip()
    if head != snap:
        raise SystemExit(f"kubernetes is at {head}, not {snap}")
    version = subprocess.run([a.graphicle, "--version"], capture_output=True, text=True).stdout.strip()
    index_info = os.path.join(work, "c4c", "index.json")
    if not os.path.exists(index_info):
        os.makedirs(data, exist_ok=True)
        t0 = time.time()
        r = subprocess.run([a.graphicle, "sync", src, "--data-dir", data, "--json"], capture_output=True, text=True)
        if r.returncode != 0:
            raise SystemExit("graphicle sync: " + r.stderr[-300:])
        with open(index_info, "w") as f:
            json.dump({"graphicle": version, "seconds": round(time.time() - t0, 1)}, f)
    packs_dir = os.path.join(work, "c4c", "packs")
    os.makedirs(packs_dir, exist_ok=True)
    counter = Counter(a.tokenizer)
    manifest = {"graphicle": version, "S": snap, "budget": a.budget, "layout": "cache", "prefix_share": SHARE,
                "counter": counter.name, "packs": []}
    for q, text in questions(a.texts, a.questions):
        pj = None
        for fmt in ("json", "xml"):
            r = subprocess.run(pack_command(a.graphicle, src, data, text, a.budget, a.tokenizer, fmt, "cache", SHARE),
                               capture_output=True, text=True, timeout=3600)
            if r.returncode != 0:
                raise SystemExit(f"{q['id']}: graphicle pack: {r.stderr[-300:]}")
            if fmt == "json":
                pj = json.loads(r.stdout)
            else:
                with open(os.path.join(packs_dir, q["id"] + ".xml"), "w", encoding="utf-8") as f:
                    f.write(r.stdout)
                manifest["packs"].append({"id": q["id"], "tokens": pj.get("tokens"), "prefix_sha256": pj.get("prefix_sha256"),
                                          "prefix_tokens": pj.get("prefix_tokens"),
                                          "xml_sha256": hashlib.sha256(r.stdout.encode("utf-8")).hexdigest(),
                                          "xml_tokens": counter.count(r.stdout)})
        print(f"{q['id']}: {manifest['packs'][-1]['tokens']} tokens, prefix {str(pj.get('prefix_sha256'))[:12]}", flush=True)
    prefixes = {p["prefix_sha256"] for p in manifest["packs"]}
    manifest["one_prefix"] = len(prefixes) == 1 and None not in prefixes
    with open(os.path.join(work, "c4c", "packs.json"), "w") as f:
        json.dump(manifest, f, indent=1)
    if not manifest["one_prefix"]:
        raise SystemExit(f"the packs have {len(prefixes)} prefixes: the session would not share one; stopped")
    print(f"packs ready: one prefix {prefixes.pop()[:12]}, budget {a.budget}")


def summarize(calls, models=(SPARK, WONDER)):
    out = {}
    for model in models:
        cs = [c for c in calls if c["model"] == model and not c.get("error")]
        if not cs:
            continue
        first, rest = cs[0], cs[1:]
        out[model] = {"calls": len(cs), "first": {k: first[k] for k in ("input_tokens", "cached_tokens", "cost_usd",
                                                                          "cached_share", "seconds")},
                      "total_usd": sum(c["cost_usd"] for c in cs), "hit5": sum(c.get("hit5", 0) for c in cs)}
        if rest:
            costs = [c["cost_usd"] for c in rest]
            shares = [c["cached_share"] for c in rest]
            out[model]["follow_ups"] = {"n": len(rest), "cost_mean": statistics.mean(costs),
                                        "cost_median": statistics.median(costs), "cost_min": min(costs),
                                        "cost_max": max(costs), "cached_share_mean": statistics.mean(shares),
                                        "cached_share_min": min(shares)}
    return out


def cmd_run(a):
    work = os.path.abspath(a.work)
    prof = PROFILES[a.profile]
    base_url = a.base_url or prof["base_url"]
    if not base_url:
        raise SystemExit(f"--base-url is needed with --profile {a.profile}")
    manifest = json.load(open(os.path.join(work, "c4c", "packs.json")))
    if not manifest.get("one_prefix"):
        raise SystemExit("the packs do not share one prefix")
    prices = json.load(open(a.prices))
    models = [(SPARK if a.profile != "mock" else "mock", N_SPARK), (WONDER if a.profile != "mock" else "mock-wonder", N_WONDER)]
    for m, _n in models:
        if m not in prices["models"]:
            raise SystemExit(f"no price for {m}")
    provider = prices["models"][models[0][0]].get("provider", a.profile)
    caps = json.load(open(a.caps))["usd"] if os.path.exists(a.caps) else {}
    global_cap = caps.get(provider)
    key = open(a.key_file).read().strip() if a.key_file else os.environ.get(prof["key_env"], "")
    if not key:
        raise SystemExit(f"no key: set {prof['key_env']} or pass --key-file")
    spend_log = os.path.abspath(a.spend_log or os.path.join(work, "spend", "spend-log.jsonl"))
    os.makedirs(os.path.join(a.out, "raw"), exist_ok=True)
    os.makedirs(os.path.dirname(spend_log), exist_ok=True)
    lock = open(spend_log + f".{provider}.lock", "w")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        sys.exit(f"REFUSED: another run for {provider} holds {lock.name}")
    counter = Counter(a.tokenizer)
    qs = questions(a.texts, a.questions)
    packs = {p["id"]: p for p in manifest["packs"]}
    prefix12 = manifest["packs"][0]["prefix_sha256"][:12]
    cache_key = f"c4c:kubernetes@{manifest['S'][:12]}:pack-cache@1M:{prefix12}"

    def prompt_of(q, text):
        body = open(os.path.join(work, "c4c", "packs", q["id"] + ".xml"), encoding="utf-8").read()
        if hashlib.sha256(body.encode("utf-8")).hexdigest() != packs[q["id"]]["xml_sha256"]:
            raise SystemExit(f"{q['id']}: the pack file changed since prepare")
        return PROMPT.format(context=body + "\n\n", repo=q["repo"], sha=q["S"], issue=text)

    def cost(price, tin, tcached, tout):
        return ((tin - tcached) * price["input"] + tcached * price.get("cached_input", price["input"])
                + tout * price["output"]) / 1e6

    # The estimate: each model's first call uncached, the others with the 90% prefix from the cache.
    est = 0.0
    for m, n in models:
        p = prices["models"][m]
        for i, (q, text) in enumerate(qs[:n]):
            tin = counter.count(prompt_of(q, text)) if a.tokenizer else packs[q["id"]]["tokens"] + 1000
            shared = packs[q["id"]].get("prefix_tokens") or int(SHARE * packs[q["id"]]["tokens"])
            est += cost(p, tin, 0 if i == 0 else min(tin, shared), OUT_TOKENS_ESTIMATE)
    calls_path = os.path.join(a.out, "calls.jsonl")
    done = [json.loads(l) for l in open(calls_path)] if os.path.exists(calls_path) else []
    logged = logged_total(spend_log, provider)
    print(f"c4c: estimate ${est:.2f}; {provider} log total ${logged:.2f}"
          + (f", cap ${global_cap:.2f}" if global_cap is not None else ""), flush=True)
    if not done and global_cap is not None and logged + est > global_cap:
        sys.exit(f"REFUSED: the {provider} log total ${logged:.2f} plus the estimate ${est:.2f} would pass the cap")
    spent_before = json.load(open(os.path.join(a.out, "spend.json")))["spent_usd"] \
        if os.path.exists(os.path.join(a.out, "spend.json")) else 0.0
    cap = spent_before + math.ceil(1.5 * est) if str(a.spend_cap).lower() == "auto" else float(a.spend_cap)
    if global_cap is not None:
        cap = min(cap, spent_before + global_cap - logged)
    spend = Spend(os.path.join(a.out, "spend.json"), cap, prices["models"][models[0][0]], spend_log, provider, global_cap)
    client = Client(base_url, key, "responses")
    run_meta = {"session": "c4c", "cache_key": cache_key, "budget": manifest["budget"], "graphicle": manifest["graphicle"],
                "S": manifest["S"], "models": [m for m, _n in models], "estimate_usd": est, "spend_cap_usd": cap,
                "global_cap_usd": global_cap, "spend_log": spend_log, "started": now_iso()}

    if not done and not a.skip_count:
        q, text = qs[0]
        t0 = time.time()
        r = client.post("/responses/input_tokens", {"model": models[0][0], "input": [user_message(prompt_of(q, text))]})
        n = r.get("input_tokens")
        # Into the ledger too; Meta's pages do not say whether counting is billed, so it is logged at $0.
        spend.settle(0.0, 0.0, {"time": now_iso(), "provider": provider, "model": models[0][0], "api": "input_tokens",
                                "arm": "c4c window check", "budget": "1M", "run": os.path.abspath(a.out),
                                "cache_key": None, "repo": "kubernetes", "question": q["id"], "turn": 0,
                                "benchmark": "c4c", "status": "ok", "input_tokens": n or 0, "cached_tokens": 0,
                                "output_tokens": 0, "reasoning_tokens": 0, "seconds": round(time.time() - t0, 3),
                                "cost_usd": 0.0})
        run_meta["window_check"] = {"input_tokens": n, "limit": WINDOW - OUT_CAP}
        print(f"window check: call 1 counts {n} input tokens (limit {WINDOW - OUT_CAP})", flush=True)
        if n is not None and n > WINDOW - OUT_CAP:
            with open(os.path.join(a.out, "run.json"), "w") as f:
                json.dump(run_meta, f, indent=1)
            sys.exit(f"STOP: call 1 would pass the window; build the packs at {SMALLER_BUDGET} "
                     f"(c4c.py prepare --budget {SMALLER_BUDGET}) and run again")
    with open(os.path.join(a.out, "run.json"), "w") as f:
        json.dump(run_meta, f, indent=1)
    have = {(c["model"], c["id"]) for c in done if not c.get("error")}
    stopped = None
    for m, n in models:
        price = prices["models"][m]
        for i, (q, text) in enumerate(qs[:n], 1):
            if (m, q["id"]) in have:
                continue
            spark_calls = [c for c in done if c["model"] == models[0][0] and not c.get("error")]
            if m == models[0][0] and i > 3 and len(spark_calls) >= 2 and \
                    spark_calls[1]["cached_tokens"] < 0.5 * spark_calls[1]["input_tokens"]:
                stopped = "Spark's second call was served less than half from the cache: stopped after its third call"
                break
            prompt = prompt_of(q, text)
            spend.price = price
            row = {"n": i, "model": m, "id": q["id"], "cache_key": cache_key, "pack_tokens": packs[q["id"]]["tokens"],
                   "error": None}
            body = {"model": m, "input": [user_message(prompt)], "store": False, "reasoning": {"effort": "medium"},
                    "max_output_tokens": OUT_CAP, "prompt_cache_key": cache_key,
                    "prompt_cache_retention": CACHE_RETENTION}
            rec = {"time": now_iso(), "provider": provider, "model": m, "api": "responses", "arm": "c4c pack-cache@1M",
                   "budget": "1M", "run": os.path.abspath(a.out), "cache_key": cache_key, "repo": "kubernetes",
                   "question": q["id"], "turn": 1, "benchmark": "c4c"}
            try:
                worst = spend.reserve(counter.count(prompt), OUT_CAP)
            except CapReached as e:
                stopped = f"cap: {e}"
                break
            t0 = time.time()
            try:
                resp = client.post("/responses", body)
            except Exception as e:
                spend.settle(worst, 0.0, dict(rec, status="error", error=str(e)[:200], input_tokens=0, cached_tokens=0,
                                              output_tokens=0, reasoning_tokens=0, seconds=round(time.time() - t0, 3),
                                              cost_usd=0.0))
                row["error"] = str(e)[:300]
                with open(calls_path, "a") as f:
                    f.write(json.dumps(row, sort_keys=True) + "\n")
                done.append(row)
                continue
            tin, tcached, tout, treason = usage_of(resp, "responses")
            c = cost(price, tin, tcached, tout)
            secs = round(time.time() - t0, 3)
            spend.settle(worst, c, dict(rec, status="ok", input_tokens=tin, cached_tokens=tcached, output_tokens=tout,
                                        reasoning_tokens=treason, seconds=secs, cost_usd=c))
            answer = text_of(resp, "responses")
            files = parse_files(answer)
            row.update(score_answer(normalize_answer(files) if files is not None else [], q["gold"]))
            row.update({"input_tokens": tin, "cached_tokens": tcached, "output_tokens": tout, "reasoning_tokens": treason,
                        "seconds": secs, "cost_usd": c, "cached_share": (tcached / tin) if tin else 0.0,
                        "answer_parsed": files is not None, "usage": resp.get("usage")})
            with open(os.path.join(a.out, "raw", f"{m}-{q['id']}.json"), "w") as f:
                json.dump({"response": resp, "answer": answer}, f)
            with open(calls_path, "a") as f:
                f.write(json.dumps(row, sort_keys=True) + "\n")
            done.append(row)
            print(f"{m} {i:2d} {q['id']}: in {tin:,} cached {tcached:,} ({row['cached_share']:.1%}) out {tout:,} "
                  f"${c:.4f}", flush=True)
        if stopped:
            break
    summary = summarize(done, [m for m, _n in models])
    summary["stopped"] = stopped
    with open(os.path.join(a.out, "summary.json"), "w") as f:
        json.dump(summary, f, indent=1)
    run_meta.update({"finished": now_iso(), "spend_usd": spend.spent, "stopped": stopped})
    with open(os.path.join(a.out, "run.json"), "w") as f:
        json.dump(run_meta, f, indent=1)
    print(json.dumps(summary, indent=1)[:3000])


def cmd_check(a):
    """The first question's pack built again by another binary, compared with the session's."""
    work = os.path.abspath(a.work)
    manifest = json.load(open(os.path.join(work, "c4c", "packs.json")))
    q, text = questions(a.texts, a.questions)[0]
    want = next(p for p in manifest["packs"] if p["id"] == q["id"])
    out = {"id": q["id"], "session_graphicle": manifest["graphicle"], "session_xml_sha256": want["xml_sha256"],
           "session_prefix_sha256": want["prefix_sha256"], "budget": manifest["budget"],
           "check_binary": os.path.abspath(a.graphicle)}
    for fmt in ("json", "xml"):
        r = subprocess.run(pack_command(a.graphicle, os.path.abspath(a.src), os.path.abspath(a.data), text,
                                        manifest["budget"], a.tokenizer, fmt, "cache", SHARE),
                           capture_output=True, text=True, timeout=3600)
        if r.returncode != 0:
            raise SystemExit(f"graphicle pack: {r.stderr[-300:]}")
        if fmt == "json":
            out["check_prefix_sha256"] = json.loads(r.stdout).get("prefix_sha256")
        else:
            out["check_xml_sha256"] = hashlib.sha256(r.stdout.encode("utf-8")).hexdigest()
    out["same"] = out["check_xml_sha256"] == want["xml_sha256"] and out["check_prefix_sha256"] == want["prefix_sha256"]
    with open(os.path.join(work, "c4c", "check-" + a.name + ".json"), "w") as f:
        json.dump(out, f, indent=1)
    print(json.dumps(out, indent=1))


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("check")
    c.add_argument("--work", required=True)
    c.add_argument("--graphicle", required=True)
    c.add_argument("--tokenizer", required=True)
    c.add_argument("--texts", required=True)
    c.add_argument("--questions", default=QUESTIONS)
    c.add_argument("--src", required=True)
    c.add_argument("--data", required=True, help="an index of --src built by --graphicle")
    c.add_argument("--name", required=True, help="names the result file WORK/c4c/check-NAME.json")
    p = sub.add_parser("prepare")
    p.add_argument("--work", required=True)
    p.add_argument("--graphicle", required=True)
    p.add_argument("--tokenizer", required=True)
    p.add_argument("--texts", required=True)
    p.add_argument("--budget", type=int, default=BUDGET)
    p.add_argument("--questions", default=QUESTIONS)
    p.add_argument("--src", help="a kubernetes checkout at S to use (default WORK/c4c/src/kubernetes, fetched)")
    p.add_argument("--data", help="its index, built by the same binary (default WORK/c4c/data/kubernetes)")
    r = sub.add_parser("run")
    r.add_argument("--work", required=True)
    r.add_argument("--out", required=True)
    r.add_argument("--texts", required=True)
    r.add_argument("--questions", default=QUESTIONS)
    r.add_argument("--profile", default="meta", choices=["meta", "mock"])
    r.add_argument("--base-url")
    r.add_argument("--key-file")
    r.add_argument("--spend-cap", default="auto")
    r.add_argument("--spend-log")
    r.add_argument("--caps", default=os.path.join(WR, "caps.json"))
    r.add_argument("--prices", default=os.path.join(WR, "prices.json"))
    r.add_argument("--tokenizer")
    r.add_argument("--skip-count", action="store_true", help="do not count call 1 with /responses/input_tokens")
    a = ap.parse_args()
    {"prepare": cmd_prepare, "run": cmd_run, "check": cmd_check}[a.cmd](a)


if __name__ == "__main__":
    main()
