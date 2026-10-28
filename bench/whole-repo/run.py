#!/usr/bin/env python3
"""Run one arm of the whole-repository benchmark against an OpenAI-compatible model API.

usage:
  run.py --arm noctx|pack|pack-cache|pack-stable|trunc|agent [--budget 128K] --work WORKDIR
         --questions DIR --texts DIR --out DIR [--profile meta|baseten|mock] [--base-url URL]
         [--model MODEL] [--api responses|chat] [--key-file FILE] [--spend-cap 20|auto]
         [--prices prices.json] [--caps caps.json] [--spend-log FILE] [--repos a,b] [--limit N]
         [--jobs 1] [--order grouped|interleaved] [--reasoning-effort medium]
         [--max-output-tokens 8192] [--tokenizer FILE] [--graphicle BIN] [--prefix-share F]
         [--pack-standin] [--max-turns 30] [--label NAME]

Profiles (--base-url, --model and --api override them):
  meta     https://api.meta.ai/v1, the Responses API, muse-spark-1.3; key from MODEL_API_KEY
  baseten  https://inference.baseten.co/v1, chat completions, meta-models/muse-spark-open (Muse Spark
           Open); key from BASETEN_API_KEY
  mock     mock_server.py at --base-url, the Responses API, model "mock"; key from MODEL_API_KEY

Arms (one prompt template for all, see PROMPT: the context first, the question last):
  noctx        the issue alone: the memorisation and contamination control
  pack         `graphicle pack "<issue>" --project P --budget B --format xml` (and --format json for
               the included files and the layout fields); --pack-standin uses `graphicle context
               --max-tokens B` instead, a stand-in for testing the plumbing, never a result
  pack-cache   the same with --layout cache --prefix-share F (--prefix-share, default 0.9): a prefix
               that is the same for every question about the repository, then a question-specific
               suffix
  pack-stable  --layout cache --prefix-share 1.0: the whole pack is the same for every question
  trunc        the repository's files in a fixed order up to B (tokens.py build_truncation)
  agent        the same model with grep, find, list_dir and read (agent_tools.py), at most --max-turns
               turns, then the same final answer (sooner when its context passes --agent-max-context);
               with --api responses the turns are chained with previous_response_id, so the model keeps
               its reasoning between turns

Caching. Every call carries a cache key made of the repository, its snapshot, the arm, the budget
and the first 12 hex digits of the sha256 of the arm's shared prefix (the pack's prefix_sha256; the
whole truncation dump; the fixed instructions for noctx and the agent); no question text is in it.
The meta and mock profiles send it as prompt_cache_key, plus prompt_cache_retention "24h" on the
Responses API; the baseten and mock profiles send its sha256 as the x-session-affinity header. With
--order grouped (the default, except for the agent) the questions of one repository run back to back
in the question file's order, the first one alone, so the others can read its prefix from the cache;
with --order interleaved (the agent's default) the repositories take turns, one question each, so a
run stopped by a cap covers them evenly. Truncation is cached exactly like the pack. The parameter and
usage names were checked on 2026-10-26 against https://dev.meta.ai/docs/protocols/responses,
https://dev.meta.ai/docs/prompt-caching, https://docs.baseten.co/inference/model-apis/pricing-and-limits.md
and https://docs.baseten.co/inference/model-apis/reasoning.md (Muse Spark Open reasons at xhigh unless
reasoning_effort says otherwise).

The key comes from the profile's environment variable or --key-file (a file holding only the key). It
is sent as a Bearer header and never printed or written. --redact strips email addresses and @handles
from each issue text before the context is built or anything is sent (redact.py); off by default.

Spending. Every call appends one line to the global spend log (--spend-log, default
WORKDIR/spend/spend-log.jsonl): time, provider, model, arm, repository, question, input, cached,
output and reasoning tokens, seconds and cost_usd at prices.json. A run refuses to start when its
provider's logged total plus the run's own estimate (estimate.py with caching, for the questions it
still has to do) would pass the provider's global cap in caps.json (Meta $45, Baseten $100), or when
another run of the same provider holds the log's lock. Before each call the run reserves the call's
largest possible cost (the prompt's tokens uncached at the input price plus --max-output-tokens at the
output price); if that would pass --spend-cap (default $20) or what is left of the global cap, the run
stops. --spend-cap auto sets the run's cap to 1.5 times its cached estimate in whole dollars, at most
what the global cap leaves (the registered rule). What a run has spent is kept in OUT/spend.json, so
a resumed run counts it.

Writes OUT/run.json (the settings, without the key), OUT/rows.jsonl (one row per question: answer,
scores, tokens, cost, time, turns) and OUT/raw/ (each question's responses and contexts' file lists,
not for publishing). Questions already in rows.jsonl are skipped, so a run can be resumed. A question
whose call fails is tried once more at the end of the run, and only a second failure stays in its row.
"""
import argparse
import datetime
import fcntl
import hashlib
import json
import math
import os
import re
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import agent_tools  # noqa: E402
from pack_arm import PACK_ARMS, pack_context  # noqa: E402
from score import normalize_answer, score_answer  # noqa: E402
from redact import redact  # noqa: E402
from tokens import Counter, build_truncation  # noqa: E402

PROMPT_VERSION = "whole-repo-v1"
PROMPT = ("{context}Repository {repo} at commit {sha}. Issue: {issue}\n\n"
          "List the source files that must change to resolve this issue, most likely first, at most 10. "
          "Answer with JSON only, in the form {{\"files\": [\"path/to/file\", ...]}}, with paths relative "
          "to the repository root.")
AGENT_CONTEXT = ("You can explore the repository at that commit with the tools grep, find, list_dir and read, "
                 "in at most {turns} turns. Then give the answer.\n\n")
LAST_TURN = "The turn limit is reached. Answer now, with the JSON only."
BUDGETS = {"64K": 65536, "128K": 131072, "256K": 262144, "512K": 524288, "1M": 1015808}
CACHE_RETENTION = "24h"
PROFILES = {
    "meta": {"base_url": "https://api.meta.ai/v1", "api": "responses", "model": "muse-spark-1.3",
             "key_env": "MODEL_API_KEY", "cache_key": True, "affinity": False},
    "baseten": {"base_url": "https://inference.baseten.co/v1", "api": "chat", "model": "meta-models/muse-spark-open",
                "key_env": "BASETEN_API_KEY", "cache_key": False, "affinity": True},
    "mock": {"base_url": None, "api": "responses", "model": "mock", "key_env": "MODEL_API_KEY",
             "cache_key": True, "affinity": True},
}


class CapReached(Exception):
    pass


def now_iso():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def sha12(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()[:12]


def logged_total(log_path, provider):
    """The cost_usd total of one provider's lines in the global spend log."""
    if not os.path.exists(log_path):
        return 0.0
    total = 0.0
    with open(log_path) as f:
        for line in f:
            if line.strip():
                r = json.loads(line)
                if r.get("provider") == provider:
                    total += r.get("cost_usd") or 0.0
    return total


class Spend:
    """What this run has spent (OUT/spend.json, for resuming) and the provider's global log and cap."""

    def __init__(self, path, cap, price, log_path, provider, global_cap):
        self.path, self.cap, self.price, self.lock = path, cap, price, threading.Lock()
        self.spent = json.load(open(path))["spent_usd"] if os.path.exists(path) else 0.0
        self.reserved = 0.0
        self.log_path, self.provider, self.global_cap = log_path, provider, global_cap
        self.logged = logged_total(log_path, provider)  # every run of the provider, this one's past calls included

    def cost(self, tin, tcached, tout):
        p = self.price
        return ((tin - tcached) * p["input"] + tcached * p.get("cached_input", p["input"]) + tout * p["output"]) / 1e6

    def reserve(self, prompt_tokens, max_out):
        worst = self.cost(prompt_tokens, 0, max_out)
        with self.lock:
            if self.spent + self.reserved + worst > self.cap:
                raise CapReached(f"run spend cap ${self.cap:.2f}: spent ${self.spent:.4f}, the next call could cost "
                                 f"${worst:.4f}")
            if self.global_cap is not None and self.logged + self.reserved + worst > self.global_cap:
                raise CapReached(f"global {self.provider} cap ${self.global_cap:.2f} (caps.json): logged "
                                 f"${self.logged:.4f}, the next call could cost ${worst:.4f}")
            self.reserved += worst
        return worst

    def settle(self, worst, actual, record):
        with self.lock:
            self.reserved -= worst
            self.spent += actual
            self.logged += actual
            with open(self.path + ".part", "w") as f:
                json.dump({"spent_usd": self.spent, "cap_usd": self.cap}, f)
            os.replace(self.path + ".part", self.path)
            with open(self.log_path, "a") as f:
                fcntl.flock(f, fcntl.LOCK_EX)
                f.write(json.dumps(record, sort_keys=True) + "\n")
                f.flush()
                fcntl.flock(f, fcntl.LOCK_UN)


class Client:
    def __init__(self, base_url, key, api):
        self.base, self.key, self.api = base_url.rstrip("/"), key, api

    def post(self, path, body, headers=None, timeout=900):
        data = json.dumps(body).encode()
        delay = 5
        for attempt in range(6):
            req = urllib.request.Request(self.base + path, data=data, method="POST", headers={
                "Content-Type": "application/json", "Authorization": "Bearer " + self.key, **(headers or {})})
            try:
                with urllib.request.urlopen(req, timeout=timeout) as r:
                    return json.loads(r.read())
            except urllib.error.HTTPError as e:
                msg = e.read()[:500].decode("utf-8", "replace")
                if e.code in (429, 500, 502, 503, 504) and attempt < 5:
                    time.sleep(delay)
                    delay *= 3
                    continue
                raise RuntimeError(f"HTTP {e.code}: {msg}")
            except (urllib.error.URLError, TimeoutError, ConnectionError) as e:
                if attempt < 5:
                    time.sleep(delay)
                    delay *= 3
                    continue
                raise RuntimeError(f"connection: {e}")


def usage_of(resp, api):
    u = resp.get("usage") or {}
    if api == "responses":
        return (u.get("input_tokens", 0), (u.get("input_tokens_details") or {}).get("cached_tokens", 0) or 0,
                u.get("output_tokens", 0), (u.get("output_tokens_details") or {}).get("reasoning_tokens", 0) or 0)
    return (u.get("prompt_tokens", 0), (u.get("prompt_tokens_details") or {}).get("cached_tokens", 0) or 0,
            u.get("completion_tokens", 0), (u.get("completion_tokens_details") or {}).get("reasoning_tokens", 0) or 0)


def text_of(resp, api):
    if api == "responses":
        parts = []
        for o in resp.get("output") or []:
            if o.get("type") == "message":
                parts.extend(c.get("text", "") for c in o.get("content") or [] if c.get("type") == "output_text")
        return "".join(parts)
    ch = (resp.get("choices") or [{}])[0].get("message") or {}
    return ch.get("content") or ""


def user_message(text):
    return {"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}


def parse_files(text):
    """The last JSON object in the text that has a "files" list."""
    dec, found = json.JSONDecoder(), None
    for m in re.finditer(r"\{", text or ""):
        try:
            obj, _ = dec.raw_decode(text, m.start())
        except json.JSONDecodeError:
            continue
        if isinstance(obj, dict) and isinstance(obj.get("files"), list):
            found = obj["files"]
    return found


def arm_label(arm, budget, prefix_share, standin=False):
    core = arm
    if arm == "pack-cache" and abs(prefix_share - PACK_ARMS["pack-cache"][1]) > 1e-9:
        core = f"pack-cache{prefix_share:g}"
    return core + ("@" + budget if budget else "") + ("-standin" if standin else "")


def estimate_arm(arm, prefix_share):
    """The arm's name in estimate.py's grids."""
    if arm == "pack-cache" and abs(prefix_share - PACK_ARMS["pack-cache"][1]) > 1e-9:
        return f"pack-cache{prefix_share:g}"
    return arm


class Runner:
    def __init__(self, a, key, prices, provider, global_cap):
        self.a = a
        self.client = Client(a.base_url, key, a.api)
        self.prof = PROFILES[a.profile]
        self.provider = provider
        self.spend = Spend(os.path.join(a.out, "spend.json"), a.spend_cap, prices["models"][a.model], a.spend_log,
                           provider, global_cap)
        self.counter = Counter(a.tokenizer)
        self.trunc_cache, self.files_cache, self.lock = {}, {}, threading.Lock()
        self.stop = threading.Event()

    def cache_fields(self, key):
        """(body fields, headers) that carry the cache key."""
        body, headers = {}, {}
        if self.prof["cache_key"]:
            body["prompt_cache_key"] = key
            if self.a.api == "responses":
                body["prompt_cache_retention"] = CACHE_RETENTION
        if self.prof["affinity"]:
            headers["x-session-affinity"] = hashlib.sha256(key.encode()).hexdigest()[:32]
        return body, headers

    def call(self, body, prompt_tokens, headers, info):
        a = self.a
        worst = self.spend.reserve(prompt_tokens, a.max_output_tokens)
        t0 = time.time()
        rec = {"time": now_iso(), "provider": self.provider, "model": a.model, "api": a.api, "arm": a.label,
               "budget": a.budget, "run": os.path.abspath(a.out), "cache_key": info.get("cache_key"),
               "repo": info["repo"], "question": info["question"], "turn": info.get("turn", 1)}
        try:
            resp = self.client.post("/responses" if a.api == "responses" else "/chat/completions", body, headers)
        except Exception as e:
            self.spend.settle(worst, 0.0, dict(rec, status="error", error=str(e)[:200], input_tokens=0,
                                               cached_tokens=0, output_tokens=0, reasoning_tokens=0,
                                               seconds=round(time.time() - t0, 3), cost_usd=0.0))
            raise
        tin, tcached, tout, treason = usage_of(resp, a.api)
        cost = self.spend.cost(tin, tcached, tout)
        secs = time.time() - t0
        self.spend.settle(worst, cost, dict(rec, status="ok", input_tokens=tin, cached_tokens=tcached,
                                            output_tokens=tout, reasoning_tokens=treason, seconds=round(secs, 3),
                                            cost_usd=cost))
        return resp, {"in": tin, "cached": tcached, "out": tout, "reasoning": treason, "cost": cost, "seconds": secs}

    def single(self, prompt, key, info):
        a = self.a
        fields, headers = self.cache_fields(key)
        if a.api == "responses":
            body = {"model": a.model, "input": [user_message(prompt)], "store": False,
                    "reasoning": {"effort": a.reasoning_effort}, "max_output_tokens": a.max_output_tokens, **fields}
        else:
            body = {"model": a.model, "messages": [{"role": "user", "content": prompt}],
                    "max_tokens": a.max_output_tokens, **fields}
            if a.reasoning_effort:
                body["reasoning_effort"] = a.reasoning_effort
        resp, u = self.call(body, self.counter.count(prompt), headers, info)
        return text_of(resp, a.api), [u], [resp], 1, 0

    def agent(self, prompt, tools, key, info):
        a = self.a
        usages, raws, ncalls = [], [], 0
        fields, headers = self.cache_fields(key)
        last_in = self.counter.count(prompt)
        if a.api == "responses":
            items, prev = [user_message(prompt)], None
            final_next = False
            for turn in range(1, a.max_turns + 2):
                body = {"model": a.model, "input": items, "store": True, "reasoning": {"effort": a.reasoning_effort},
                        "max_output_tokens": a.max_output_tokens, **fields}
                if not final_next:
                    body["tools"] = agent_tools.responses_tools()
                if prev:
                    body["previous_response_id"] = prev
                resp, u = self.call(body, last_in + sum(self.counter.count(str(i.get("output", i.get("content", ""))))
                                                        for i in items), headers, dict(info, turn=turn))
                usages.append(u)
                raws.append(resp)
                last_in = u["in"] + u["out"]
                calls = [o for o in resp.get("output") or [] if o.get("type") == "function_call"]
                if not calls or final_next:
                    return text_of(resp, a.api), usages, raws, turn, ncalls
                items = []
                for c in calls:
                    ncalls += 1
                    try:
                        args = json.loads(c.get("arguments") or "{}")
                    except json.JSONDecodeError:
                        args = {}
                    items.append({"type": "function_call_output", "call_id": c["call_id"],
                                  "output": tools.call(c.get("name"), args)})
                if turn == a.max_turns or last_in > a.agent_max_context:
                    items.append(user_message(LAST_TURN))
                    final_next = True
                prev = resp["id"]
        else:
            msgs, final_next = [{"role": "user", "content": prompt}], False
            for turn in range(1, a.max_turns + 2):
                body = {"model": a.model, "messages": msgs, "max_tokens": a.max_output_tokens, **fields}
                if a.reasoning_effort:
                    body["reasoning_effort"] = a.reasoning_effort
                if not final_next:
                    body["tools"] = agent_tools.chat_tools()
                resp, u = self.call(body, self.counter.count(json.dumps(msgs)), headers, dict(info, turn=turn))
                usages.append(u)
                raws.append(resp)
                msg = (resp.get("choices") or [{}])[0].get("message") or {}
                calls = msg.get("tool_calls") or []
                msgs.append({k: v for k, v in msg.items() if k in ("role", "content", "tool_calls")})
                if not calls or final_next:
                    return msg.get("content") or "", usages, raws, turn, ncalls
                for c in calls:
                    ncalls += 1
                    try:
                        args = json.loads(c["function"].get("arguments") or "{}")
                    except json.JSONDecodeError:
                        args = {}
                    msgs.append({"role": "tool", "tool_call_id": c["id"],
                                 "content": tools.call(c["function"].get("name"), args)})
                if turn == a.max_turns or u["in"] + u["out"] > a.agent_max_context:
                    msgs.append({"role": "user", "content": LAST_TURN})
                    final_next = True
        return "", usages, raws, a.max_turns + 1, ncalls

    def context(self, name, q, text):
        """(context text, included paths or None, context tokens, shared-prefix sha256 or None, pack info)."""
        a = self.a
        if a.arm == "noctx":
            return "", None, 0, hashlib.sha256(PROMPT.encode()).hexdigest(), {}
        if a.arm == "agent":
            ctx = AGENT_CONTEXT.format(turns=a.max_turns)
            return ctx, None, 0, hashlib.sha256(ctx.encode()).hexdigest(), {}
        if a.arm == "trunc":
            with self.lock:
                if name not in self.trunc_cache:
                    t, inc, ntok = build_truncation(a.work, name, a.budget_tokens, self.counter)
                    self.trunc_cache[name] = (t, inc, ntok, hashlib.sha256(t.encode("utf-8")).hexdigest())
            ctx, included, ntok, sha = self.trunc_cache[name]
            return ctx + "\n\n", included, ntok, sha, {}
        layout, share = PACK_ARMS[a.arm]
        if a.arm == "pack-cache":
            share = a.prefix_share
        p = pack_context(a.graphicle, os.path.join(a.work, "src", name), os.path.join(a.work, "data", name), text,
                         a.budget_tokens, a.tokenizer, a.pack_standin, layout=layout, prefix_share=share)
        info = {k: p.get(k) for k in ("layout", "prefix_share", "prefix_tokens", "prefix_sha256", "prefix_ok",
                                      "suffix_tokens")}
        if layout and p.get("prefix_ok") is False:
            raise RuntimeError("the pack's prefix does not match its prefix_sha256 or does not come first")
        return p["text"] + "\n\n", p["included"], p["tokens"], p.get("prefix_sha256"), info

    def tools_for(self, name):
        """Fresh tools for one question (their read list is the agent's context), sharing the file list."""
        src = os.path.join(self.a.work, "src", name)
        with self.lock:
            if name not in self.files_cache:
                self.files_cache[name] = agent_tools.RepoTools(src).files
        return agent_tools.RepoTools(src, self.files_cache[name])

    def one(self, q, text):
        a = self.a
        name = q["id"].rsplit("-", 1)[0]
        row = {"id": q["id"], "repo": name, "arm": a.arm, "label": a.label, "budget": a.budget,
               "budget_tokens": a.budget_tokens, "model": a.model, "api": a.api, "profile": a.profile,
               "gold": q["gold"], "names_gold_path": q["names_gold_path"], "error": None}
        if a.redact:
            # Emails and @handles out of the issue text before anything is built or sent (redact.py).
            text, n_emails, n_handles = redact(text)
            row["redacted"] = {"emails": n_emails, "handles": n_handles}
        t0 = time.time()
        try:
            ctx, included, ntok, prefix_sha, pinfo = self.context(name, q, text)
            key = f"wrb:{name}@{q['S'][:12]}:{a.label}:{(prefix_sha or 'none')[:12]}"
            row.update({"cache_key": key, **pinfo})
            prompt = PROMPT.format(context=ctx, repo=q["repo"], sha=q["S"], issue=text)
            tools = self.tools_for(name)
            info = {"repo": name, "question": q["id"], "cache_key": key}
            if a.arm == "agent":
                answer, usages, raws, turns, ncalls = self.agent(prompt, tools, key, info)
                included = tools.read_paths
            else:
                answer, usages, raws, turns, ncalls = self.single(prompt, key, info)
            files = parse_files(answer)
            norm = normalize_answer(files, tools.file_set) if files is not None else []
            row.update(score_answer(norm, q["gold"]))
            row.update({"files": norm, "answer_parsed": files is not None, "context_tokens": ntok,
                        "context_files": len(included) if included is not None else None,
                        "context_recall": (sum(1 for g in q["gold"] if g in set(included)) / len(q["gold"])
                                           if included is not None else None),
                        "tokens_in": sum(u["in"] for u in usages), "tokens_cached": sum(u["cached"] for u in usages),
                        "tokens_out": sum(u["out"] for u in usages),
                        "tokens_reasoning": sum(u["reasoning"] for u in usages),
                        "cached_per_call": [u["cached"] for u in usages],
                        "cost_usd": sum(u["cost"] for u in usages), "turns": turns, "tool_calls": ncalls,
                        "prompt_tokens_counted": self.counter.count(prompt) if a.arm != "agent" else None})
            with open(os.path.join(a.out, "raw", q["id"] + ".json"), "w") as f:
                json.dump({"responses": raws, "included": included, "answer": answer}, f)
        except CapReached:
            self.stop.set()
            raise
        except Exception as e:
            row["error"] = f"{type(e).__name__}: {str(e)[:300]}"
        row["wall_seconds"] = round(time.time() - t0, 2)
        return row


def bench_commit():
    """The commit of this code: BENCH_COMMIT beside run.py (written when the folder is exported), else git."""
    path = os.path.join(HERE, "BENCH_COMMIT")
    if os.path.exists(path):
        with open(path) as f:
            return f.read().strip()
    r = subprocess.run(["git", "rev-parse", "HEAD"], cwd=HERE, capture_output=True, text=True)
    return r.stdout.strip() or None


def interleave(groups):
    """Round robin over the groups: the first question of each, then the second of each, ..."""
    out, i = [], 0
    while any(i < len(items) for _n, items in groups):
        out.extend(items[i] for _n, items in groups if i < len(items))
        i += 1
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--arm", required=True, choices=["noctx", "pack", "pack-cache", "pack-stable", "trunc", "agent"])
    ap.add_argument("--budget", default=None, choices=list(BUDGETS))
    ap.add_argument("--work", required=True)
    ap.add_argument("--questions", required=True)
    ap.add_argument("--texts", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--profile", default="meta", choices=list(PROFILES))
    ap.add_argument("--base-url")
    ap.add_argument("--model")
    ap.add_argument("--api", choices=["responses", "chat"])
    ap.add_argument("--key-file")
    ap.add_argument("--spend-cap", default="20",
                    help="dollars (default 20), or auto: 1.5 x the run's cached estimate in whole dollars, at most "
                         "what the global cap leaves")
    ap.add_argument("--prices", default=os.path.join(HERE, "prices.json"))
    ap.add_argument("--caps", default=os.path.join(HERE, "caps.json"))
    ap.add_argument("--spend-log", help="the global spend log (default WORKDIR/spend/spend-log.jsonl)")
    ap.add_argument("--repos", default="")
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--jobs", type=int, default=1)
    ap.add_argument("--order", choices=["grouped", "interleaved"])
    ap.add_argument("--reasoning-effort", default="medium")
    ap.add_argument("--max-output-tokens", type=int, default=8192)
    ap.add_argument("--max-turns", type=int, default=30)
    ap.add_argument("--agent-max-context", type=int, default=900000,
                    help="the agent answers at once when a turn's context passes this many tokens")
    ap.add_argument("--tokenizer")
    ap.add_argument("--graphicle", default="graphicle")
    ap.add_argument("--prefix-share", type=float, default=PACK_ARMS["pack-cache"][1],
                    help="the cache layout's prefix share for --arm pack-cache")
    ap.add_argument("--pack-standin", action="store_true")
    ap.add_argument("--label")
    ap.add_argument("--redact", action="store_true",
                    help="strip email addresses and @handles from the issue text first (redact.py)")
    a = ap.parse_args()
    if a.arm in ("pack", "pack-cache", "pack-stable", "trunc") and not a.budget:
        raise SystemExit("--budget is needed for the pack and truncation arms")
    if a.pack_standin and a.arm != "pack":
        raise SystemExit("--pack-standin goes with --arm pack only")
    prof = PROFILES[a.profile]
    a.base_url = a.base_url or prof["base_url"]
    a.model = a.model or prof["model"]
    a.api = a.api or prof["api"]
    a.order = a.order or ("interleaved" if a.arm == "agent" else "grouped")
    if not a.base_url:
        raise SystemExit(f"--base-url is needed with --profile {a.profile}")
    a.budget_tokens = BUDGETS[a.budget] if a.budget else None
    a.work = os.path.abspath(a.work)
    a.spend_log = os.path.abspath(a.spend_log or os.path.join(a.work, "spend", "spend-log.jsonl"))
    a.label = a.label or arm_label(a.arm, a.budget, a.prefix_share, a.pack_standin)
    prices = json.load(open(a.prices))
    if a.model not in prices["models"]:
        raise SystemExit(f"no price for {a.model} in {a.prices}")
    provider = prices["models"][a.model].get("provider", a.profile)
    caps = json.load(open(a.caps))["usd"] if os.path.exists(a.caps) else {}
    global_cap = caps.get(provider)
    key = os.environ.get(prof["key_env"], "")
    if a.key_file:
        key = open(a.key_file).read().strip()
    if not key:
        raise SystemExit(f"no key: set {prof['key_env']} or pass --key-file")
    os.makedirs(os.path.join(a.out, "raw"), exist_ok=True)
    os.makedirs(os.path.dirname(a.spend_log), exist_ok=True)

    # One run per provider at a time, so the global cap is checked against everything in flight.
    lock = open(a.spend_log + f".{provider}.lock", "w")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        print(f"REFUSED: another run for {provider} holds {lock.name}", file=sys.stderr)
        sys.exit(3)

    repos = [r for r in a.repos.split(",") if r]
    groups = []
    for path in sorted(os.listdir(a.questions)):
        if not (path.startswith("questions-") and path.endswith(".jsonl")):
            continue
        name = path[len("questions-"):-len(".jsonl")]
        if repos and name not in repos:
            continue
        texts = {}
        for line in open(os.path.join(a.texts, f"texts-{name}.jsonl")):
            t = json.loads(line)
            texts[t["id"]] = t["text"]
        qs = [json.loads(l) for l in open(os.path.join(a.questions, path)) if l.strip()]
        if a.limit:
            qs = qs[:a.limit]
        groups.append((name, [(q, texts[q["id"]]) for q in qs]))
    rows_path = os.path.join(a.out, "rows.jsonl")
    done = set()
    if os.path.exists(rows_path):
        done = {json.loads(l)["id"] for l in open(rows_path) if l.strip()}
    total_q = sum(len(items) for _n, items in groups)
    groups = [(n, [(q, t) for q, t in items if q["id"] not in done]) for n, items in groups]
    left = sum(len(items) for _n, items in groups)

    # The run's own estimate (estimate.py, with caching) for the questions it still has to do.
    import estimate
    est = estimate.run_estimate(a.questions, a.texts, estimate_arm(a.arm, a.prefix_share), a.budget,
                                [n for n, _ in groups], a.limit, a.model, prices)
    frac = left / total_q if total_q else 0.0
    est_left = {k: v * frac for k, v in est.items()}
    logged = logged_total(a.spend_log, provider)
    print(f"{a.label}: {left} of {total_q} questions to do; estimate ${est_left['cached']:.2f} with caching "
          f"(${est_left['uncached']:.2f} without); {provider} log total ${logged:.2f}"
          + (f", global cap ${global_cap:.2f}" if global_cap is not None else ", no global cap"), flush=True)
    if global_cap is not None and logged + est_left["cached"] > global_cap:
        print(f"REFUSED: the {provider} log total ${logged:.2f} plus this run's estimate ${est_left['cached']:.2f} "
              f"would pass the global cap ${global_cap:.2f} ({a.caps})", file=sys.stderr)
        sys.exit(3)
    if str(a.spend_cap).lower() == "auto":
        # 1.5 x the cached estimate of what is left, in whole dollars, never past what the global cap leaves.
        spent_before = 0.0
        if os.path.exists(os.path.join(a.out, "spend.json")):
            with open(os.path.join(a.out, "spend.json")) as f:
                spent_before = json.load(f)["spent_usd"]
        allow = float(math.ceil(1.5 * est_left["cached"]))
        if global_cap is not None:
            allow = min(allow, global_cap - logged)
        a.spend_cap = spent_before + max(allow, 0.0)
        print(f"{a.label}: spend cap auto = ${a.spend_cap:.2f}", flush=True)
    else:
        a.spend_cap = float(a.spend_cap)

    runner = Runner(a, key, prices, provider, global_cap)
    graphicle_version = ""
    if a.arm.startswith("pack"):
        graphicle_version = subprocess.run([a.graphicle, "--version"], capture_output=True, text=True).stdout.strip()
    meta = {"label": a.label, "arm": a.arm, "budget": a.budget, "budget_tokens": a.budget_tokens, "model": a.model,
            "api": a.api, "profile": a.profile, "provider": provider, "base_url": a.base_url,
            "reasoning_effort": a.reasoning_effort, "max_output_tokens": a.max_output_tokens,
            "max_turns": a.max_turns if a.arm == "agent" else None, "order": a.order, "jobs": a.jobs,
            "prefix_share": a.prefix_share if a.arm == "pack-cache" else PACK_ARMS.get(a.arm, (None, None))[1],
            "cache_key": "prompt_cache_key" if PROFILES[a.profile]["cache_key"] else None,
            "cache_retention": CACHE_RETENTION if PROFILES[a.profile]["cache_key"] and a.api == "responses" else None,
            "session_affinity": PROFILES[a.profile]["affinity"],
            "counter": runner.counter.name, "prompt_version": PROMPT_VERSION, "graphicle": graphicle_version,
            "context_source": ("graphicle context (stand-in, not a result)" if a.pack_standin else
                               "graphicle pack" if a.arm.startswith("pack") else a.arm),
            "spend_cap_usd": a.spend_cap, "global_cap_usd": global_cap, "spend_log": a.spend_log,
            "estimate_usd": est_left, "bench_commit": bench_commit(), "redact": a.redact, "started": now_iso()}
    wlock = threading.Lock()
    stopped = None
    retry = []

    def write(row):
        with wlock:
            with open(rows_path, "a") as f:
                f.write(json.dumps(row, sort_keys=True) + "\n")

    def work(item, last_try=False):
        nonlocal stopped
        if runner.stop.is_set():
            return
        q, t = item
        try:
            row = runner.one(q, t)
        except CapReached as e:
            stopped = str(e)
            return
        if row["error"] and not last_try:
            with wlock:
                retry.append(item)
            return
        write(row)

    if a.order == "grouped":
        for _name, items in groups:
            if runner.stop.is_set() or not items:
                continue
            work(items[0])  # alone, so the others can read its prefix from the cache
            with ThreadPoolExecutor(max(1, a.jobs)) as ex:
                list(ex.map(work, items[1:]))
    else:
        with ThreadPoolExecutor(max(1, a.jobs)) as ex:
            list(ex.map(work, interleave(groups)))
    for item in list(retry):
        work(item, last_try=True)
    meta["finished"] = now_iso()
    meta["spend_usd"] = runner.spend.spent
    meta["stopped"] = stopped
    meta["retried"] = len(retry)
    with open(os.path.join(a.out, "run.json"), "w") as f:
        json.dump(meta, f, indent=1, sort_keys=True)
    n = sum(1 for _ in open(rows_path)) if os.path.exists(rows_path) else 0
    print(f"{a.label}: {n} rows, spent ${runner.spend.spent:.4f}; {provider} log total ${runner.spend.logged:.4f}"
          + (f"; STOPPED: {stopped}" if stopped else ""))


if __name__ == "__main__":
    main()
