#!/usr/bin/env python3
"""Muse Code headless, with and without Graphicle's plugin, on WholeRepoBench questions.

usage: muse_bench.py --arm plain|plugin --work WORK --texts DIR --out OUT [--questions DIR]
         [--repos kubernetes,home-assistant] [--first N] [--only ID,ID] [--muse PATH] [--graphicle PATH]
         [--model muse-spark-1.3] [--max-steps 25] [--run-cap 1.0] [--day-cap 10.0] [--margin 0.25]
         [--spend-log FILE] [--caps caps.json] [--prices prices.json] [--key-file FILE]
         [--state DIR] [--timeout 1200]

Each question runs `muse exec --json` once, in a clone of WORK/src/<repo> at the question's snapshot S. The
clone shares the bench checkout's objects (git clone --shared) and never changes it; it is reset and cleaned
before every question. Each question gets a fresh copy of its arm's HOME, so Muse Code's own memory and
session state never carry from one question to the next:
- plain: Muse Code alone, with nothing of Graphicle's;
- plugin: Graphicle's plugin installed (graphicle connect muse --hooks --allow-learning) and approved, without
  its three capture hooks (Stop, PreCompact, SessionEnd), so no question's session becomes a source for the next.
The prompt is the benchmark's own (run.py's PROMPT with no context: list the files to change, JSON only), the
same in both arms, with Contributor mode off.

Measures per question: the gold files named (score.py's hit@1/3/5, all-gold@5 and reciprocal rank, on the
answer's "files"), model calls, input, cached and output tokens from Muse Code's usage events, the cost at
prices.json's list prices, and the wall time.

Caps (this kit's spending limit: $1 a run and $10 a day by default, $15 in all in caps.json): a run starts only
when the day's logged total plus the run cap stays within the day cap, and the log's total plus the run cap
within the total cap.
While a run streams, its usage is added up and the run is stopped (SIGTERM to its process group, then SIGKILL)
once its cost reaches the run cap less --margin, the room left for a call in flight; --max-model-steps bounds
it as well. Every run appends one line to this kit's spend log (provider meta, kit muse-code), whatever its
outcome; the log is separate from the whole-repository benchmark's spend log. The key is read
from --key-file (or MODEL_API_KEY) and given to Muse Code as META_API_KEY; it is
never printed or written.
"""
import argparse
import datetime
import fcntl
import json
import os
import shutil
import signal
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
BENCH = os.path.join(HERE, "..", "whole-repo")
sys.path.insert(0, BENCH)
from run import PROMPT, parse_files  # noqa: E402
from score import normalize_answer, score_answer  # noqa: E402

KIT = "muse-code"
CAPTURE_HOOKS = ("stop", "pre-compact", "session-end")


class CapReached(Exception):
    pass


def now_iso():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def logged(log_path, provider, kit=None, day=None):
    """The cost_usd total of the spend log's lines of `provider` (and `kit`, on UTC `day`)."""
    total = 0.0
    if not os.path.exists(log_path):
        return total
    with open(log_path) as f:
        for line in f:
            if not line.strip():
                continue
            r = json.loads(line)
            if r.get("provider") != provider:
                continue
            if kit is not None and r.get("kit") != kit:
                continue
            if day is not None and not str(r.get("time", "")).startswith(day):
                continue
            total += r.get("cost_usd") or 0.0
    return total


def append_log(log_path, record):
    os.makedirs(os.path.dirname(log_path), exist_ok=True)
    with open(log_path, "a") as f:
        fcntl.flock(f, fcntl.LOCK_EX)
        f.write(json.dumps(record, sort_keys=True) + "\n")
        f.flush()
        fcntl.flock(f, fcntl.LOCK_UN)


def tokens_of(obj):
    """(input, cached, output, reasoning) of a usage object, or None when it is not one."""
    if not isinstance(obj, dict):
        return None
    tin = obj.get("input_tokens", obj.get("prompt_tokens"))
    tout = obj.get("output_tokens", obj.get("completion_tokens"))
    if not isinstance(tin, int) or not isinstance(tout, int):
        return None
    cached = obj.get("cached_input_tokens", obj.get("cached_tokens"))
    if cached is None:
        details = obj.get("input_tokens_details") or obj.get("prompt_tokens_details") or {}
        cached = details.get("cached_tokens", 0) if isinstance(details, dict) else 0
    if not isinstance(cached, int) and isinstance(obj.get("non_cached_input_tokens"), int):
        cached = tin - obj["non_cached_input_tokens"]
    reasoning = obj.get("reasoning_tokens")
    if reasoning is None:
        details = obj.get("output_tokens_details") or {}
        reasoning = details.get("reasoning_tokens", 0) if isinstance(details, dict) else 0
    return tin, int(cached or 0), tout, int(reasoning or 0)


def walk(value):
    stack = [value]
    while stack:
        v = stack.pop()
        if isinstance(v, dict):
            yield v
            stack.extend(v.values())
        elif isinstance(v, list):
            stack.extend(v)


def event_name(event):
    if not isinstance(event, dict):
        return ""
    for key in ("type", "event", "kind", "method"):
        if isinstance(event.get(key), str):
            return event[key]
    return ""


class Meter:
    """Usage and answer text from Muse Code's --json events, as they stream."""

    def __init__(self, price):
        self.price = price
        self.calls = []          # per-call usage tuples
        self.seen = set()        # response ids already counted
        self.total = None        # run.usage aggregate, when Muse Code printed one
        self.texts = []          # assistant texts, in order
        self.events = 0

    def cost_of(self, tin, cached, tout):
        p = self.price
        return ((tin - cached) * p["input"] + cached * p.get("cached_input", p["input"]) + tout * p["output"]) / 1e6

    def feed(self, event):
        self.events += 1
        name = event_name(event)
        if "run.usage" in name or (name.endswith("usage") and "run" in name):
            found = [t for t in (tokens_of(obj) for obj in walk(event)) if t]
            if found:
                # A total beside per-model rows is the sum of the others: use it alone.
                big = max(found, key=lambda t: t[0])
                rest = [t for t in found if t is not big]
                if rest and all(sum(t[i] for t in rest) == big[i] for i in range(4)):
                    self.total = big
                else:
                    self.total = tuple(sum(t[i] for t in found) for i in range(4))
            return
        for obj in walk(event):
            usage = obj.get("usage")
            t = tokens_of(usage) if isinstance(usage, dict) else None
            if t:
                rid = obj.get("response_id")
                if not (rid and rid in self.seen):
                    if rid:
                        self.seen.add(rid)
                    self.calls.append(t)
            kind = obj.get("kind") or obj.get("type") or ""
            if isinstance(kind, str) and (kind in ("assistant_message_committed", "assistant_message",
                                                   "agent_message", "final_message")
                                          or kind.endswith("message.completed")):
                text = obj.get("text") or obj.get("message") or obj.get("content")
                if isinstance(text, list):
                    text = "\n".join(p.get("text", "") for p in text if isinstance(p, dict))
                if isinstance(text, str) and text.strip():
                    self.texts.append(text)

    def usage(self):
        if self.total is not None and self.total[0] > 0:
            return self.total
        return tuple(sum(c[i] for c in self.calls) for i in range(4))

    def spent(self):
        tin, cached, tout, _ = self.usage()
        return self.cost_of(tin, cached, tout)


def stop(proc):
    try:
        os.killpg(proc.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        proc.wait(timeout=20)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def git(*args, cwd=None):
    subprocess.run(["git", *args], cwd=cwd, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def clone_at(src, dest, sha):
    """A clone of src that shares its objects, at sha, with every untracked file removed."""
    if not os.path.isdir(os.path.join(dest, ".git")):
        os.makedirs(os.path.dirname(dest), exist_ok=True)
        git("clone", "-q", "--shared", "--no-checkout", src, dest)
    git("checkout", "-q", "-f", "--detach", sha, cwd=dest)
    git("clean", "-q", "-fdx", cwd=dest)


def child_env(home, key):
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": home, "LANG": "C.UTF-8",
           "XDG_CONFIG_HOME": os.path.join(home, ".config"), "XDG_DATA_HOME": os.path.join(home, ".local/share"),
           "XDG_CACHE_HOME": os.path.join(home, ".cache"), "TMPDIR": os.path.join(home, "tmp"),
           "NO_COLOR": "1"}
    os.makedirs(env["TMPDIR"], exist_ok=True)
    if key:
        env["META_API_KEY"] = key
    return env


def prepare_plugin_home(a, home, clone, key):
    """The plugin arm's HOME: Graphicle synced and its plugin installed and approved, capture hooks removed."""
    env = child_env(home, key)
    data = os.path.join(a.state, "graphicle-data")
    subprocess.run([a.graphicle, "--data-dir", data, "setup", "--yes", "--clients", "none", "--no-instructions",
                    "--no-hooks", "--json"], cwd=clone, env=env, check=True, stdout=subprocess.DEVNULL)
    subprocess.run([a.graphicle, "--data-dir", data, "connect", "muse", "--hooks", "--allow-learning", "--json"],
                   cwd=clone, env=env, check=True, stdout=subprocess.DEVNULL)
    manifest_path = os.path.join(data, "muse-code-plugin", ".muse-plugin", "plugin.json")
    manifest = json.load(open(manifest_path))
    hooks = manifest["capabilities"]["hooks"]
    manifest["capabilities"]["hooks"] = [h for h in hooks if h["command"][-1] not in CAPTURE_HOOKS]
    with open(manifest_path, "w") as f:
        json.dump(manifest, f, indent=2)
    subprocess.run([a.muse, "plugins", "update", "graphicle", "--json"], env=env, check=True,
                   stdout=subprocess.DEVNULL)
    subprocess.run([a.muse, "plugins", "approve", "graphicle", "--json"], env=env, check=True,
                   stdout=subprocess.DEVNULL)


def run_question(a, q, text, clone, template, key, price, out):
    qid = q["id"]
    home = os.path.join(a.state, "homes", f"{a.arm}-{qid}")
    if os.path.exists(home):
        shutil.rmtree(home)
    shutil.copytree(template, home, symlinks=True)
    prompt = PROMPT.format(context="", repo=q["repo"], sha=q["S"], issue=text)
    prompt_file = os.path.join(home, "prompt.txt")
    with open(prompt_file, "w") as f:
        f.write(prompt)
    cmd = [a.muse, "exec", "--json", "--model", a.model, "--contributor-mode", "off", "--disable-approval",
           "--disable-request-user-input", "--trust-workspace", "--max-model-steps", str(a.max_steps),
           "--prompt-file", prompt_file]
    meter = Meter(price)
    raw_path = os.path.join(out, "raw", f"{qid}.jsonl")
    reason, t0 = None, time.time()
    with open(raw_path, "w") as raw, open(os.path.join(out, "raw", f"{qid}.stderr"), "w") as err:
        proc = subprocess.Popen(cmd, cwd=clone, env=child_env(home, key), stdout=subprocess.PIPE, stderr=err,
                                stdin=subprocess.DEVNULL, start_new_session=True, text=True)
        timer = threading.Timer(a.timeout, lambda: stop(proc))
        timer.start()
        try:
            for line in proc.stdout:
                raw.write(line)
                try:
                    meter.feed(json.loads(line))
                except ValueError:
                    continue
                if reason is None and meter.spent() >= a.run_cap - a.margin:
                    reason = f"run cap: ${meter.spent():.4f} of ${a.run_cap:.2f} less ${a.margin:.2f}"
                    stop(proc)
            code = proc.wait()
        finally:
            timer.cancel()
    seconds = time.time() - t0
    if reason is None and seconds >= a.timeout:
        reason = f"timeout {a.timeout}s"
    tin, cached, tout, treason = meter.usage()
    cost = meter.cost_of(tin, cached, tout)
    answer = "\n".join(meter.texts)
    files = parse_files(answer)
    tracked = subprocess.run(["git", "ls-files", "-z"], cwd=clone, capture_output=True, text=True).stdout
    file_set = {p for p in tracked.split("\0") if p}
    norm = normalize_answer(files, file_set) if files is not None else []
    row = {"id": qid, "repo": q["repo"], "arm": a.arm, "model": a.model, "gold": q["gold"], "files": norm,
           "answer_parsed": files is not None, "exit_code": code, "stopped": reason, "events": meter.events,
           "model_calls": len(meter.calls), "tokens_in": tin, "tokens_cached": cached, "tokens_out": tout,
           "tokens_reasoning": treason, "cost_usd": round(cost, 6), "wall_seconds": round(seconds, 1)}
    row.update(score_answer(norm, q["gold"]))
    append_log(a.spend_log, {"time": now_iso(), "provider": "meta", "kit": KIT, "client": "muse-code",
                             "model": a.model, "arm": f"muse-{a.arm}", "run": os.path.abspath(out),
                             "repo": q["repo"].split("/")[-1], "question": qid, "status": "ok" if code == 0 and
                             not reason else "stopped" if reason else "error", "input_tokens": tin,
                             "cached_tokens": cached, "output_tokens": tout, "reasoning_tokens": treason,
                             "model_calls": len(meter.calls), "seconds": round(seconds, 3),
                             "cost_usd": round(cost, 6)})
    return row


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--arm", choices=["plain", "plugin"], required=True)
    ap.add_argument("--work", required=True, help="the bench folder holding src/<repo> checkouts")
    ap.add_argument("--texts", required=True, help="the folder of texts-<repo>.jsonl")
    ap.add_argument("--out", required=True)
    ap.add_argument("--questions", default=os.path.join(BENCH, "questions"))
    ap.add_argument("--repos", default="kubernetes,home-assistant")
    ap.add_argument("--first", type=int, default=5, help="the first N questions of each repository's file")
    ap.add_argument("--only", default="", help="question ids instead of --first")
    ap.add_argument("--muse", default="muse")
    ap.add_argument("--graphicle", default="graphicle")
    ap.add_argument("--model", default="muse-spark-1.3")
    ap.add_argument("--max-steps", type=int, default=25)
    ap.add_argument("--run-cap", type=float, default=1.0)
    ap.add_argument("--day-cap", type=float, default=10.0)
    ap.add_argument("--margin", type=float, default=0.25)
    ap.add_argument("--spend-log", required=True, help="this kit's spend log (JSON lines)")
    ap.add_argument("--caps", default=os.path.join(HERE, "caps.json"), help="this kit's total cap")
    ap.add_argument("--prices", default=os.path.join(BENCH, "prices.json"))
    ap.add_argument("--key-file", default=None)
    ap.add_argument("--state", default=None, help="clones and HOMEs (default OUT/../state)")
    ap.add_argument("--timeout", type=int, default=1200)
    a = ap.parse_args()
    a.spend_log = os.path.abspath(a.spend_log)
    a.state = os.path.abspath(a.state or os.path.join(a.out, "..", "state"))
    price = json.load(open(a.prices))["models"][a.model]
    global_cap = json.load(open(a.caps))["usd"].get("meta")
    key = None
    if a.key_file:
        key = open(a.key_file).read().strip()
    else:
        key = os.environ.get("MODEL_API_KEY") or os.environ.get("META_API_KEY")
    if not key:
        sys.exit("muse_bench: no key: pass --key-file (a 0600 file) or set MODEL_API_KEY")
    os.makedirs(os.path.join(a.out, "raw"), exist_ok=True)
    os.makedirs(a.state, exist_ok=True)
    only = [x for x in a.only.split(",") if x]
    rows_path = os.path.join(a.out, "rows.jsonl")
    done = set()
    if os.path.exists(rows_path):
        done = {json.loads(l)["id"] for l in open(rows_path) if l.strip()}
    for name in [r for r in a.repos.split(",") if r]:
        qs = [json.loads(l) for l in open(os.path.join(a.questions, f"questions-{name}.jsonl")) if l.strip()]
        qs = [q for q in qs if q["id"] in only] if only else qs[:a.first]
        texts = {}
        for line in open(os.path.join(a.texts, f"texts-{name}.jsonl")):
            t = json.loads(line)
            texts[t["id"]] = t["text"]
        clone = os.path.join(a.state, "clones", f"{a.arm}-{name}")
        template = os.path.join(a.state, "homes", f"{a.arm}-template-{name}")
        for q in qs:
            if q["id"] in done:
                continue
            day = now_iso()[:10]
            today = logged(a.spend_log, "meta", KIT, day)
            if today + a.run_cap > a.day_cap:
                raise CapReached(f"day cap ${a.day_cap:.2f}: logged ${today:.4f} today")
            if global_cap is not None and logged(a.spend_log, "meta") + a.run_cap > global_cap:
                raise CapReached(f"total cap ${global_cap:.2f} (caps.json)")
            clone_at(os.path.join(a.work, "src", name), clone, q["S"])
            if not os.path.isdir(template):
                os.makedirs(template)
                if a.arm == "plugin":
                    prepare_plugin_home(a, template, clone, key)
            row = run_question(a, q, texts[q["id"]], clone, template, key, price, a.out)
            with open(rows_path, "a") as f:
                f.write(json.dumps(row, sort_keys=True) + "\n")
            print(f"{q['id']} {a.arm}: hit5={row['hit5']} calls={row['model_calls']} in={row['tokens_in']} "
                  f"cached={row['tokens_cached']} out={row['tokens_out']} ${row['cost_usd']:.4f}"
                  f"{' STOPPED ' + row['stopped'] if row['stopped'] else ''}", flush=True)


if __name__ == "__main__":
    try:
        main()
    except CapReached as e:
        sys.exit(f"muse_bench: stopped by a cap: {e}")
