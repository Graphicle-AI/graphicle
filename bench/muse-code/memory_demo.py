#!/usr/bin/env python3
"""Session memory in Muse Code, in two headless sessions on a public repository.

usage: memory_demo.py --repo DIR --data DIR --out DIR [--muse muse] [--graphicle graphicle]
         [--model muse-spark-1.3] [--key-file FILE] [--home DIR] [--approve] [--spend-log FILE]
         [--run-cap 1.0] [--day-cap 10.0] [--decision TEXT] [--question TEXT]

1. Setup: in the repository (Graphicle's data folder --data, Muse Code's HOME --home), `graphicle setup
   --clients none` syncs it and `graphicle connect muse --hooks --allow-learning` installs Graphicle's plugin;
   then `muse plugins approve graphicle`.
2. Session 1 makes a decision: the prompt states it and asks Muse Code to record it with Graphicle's
   propose_memory tool, quoting the code it rests on. The proposal is a draft.
3. Approval: you approve the draft in the app (`graphicle --data-dir DATA open`, Memories, Review draft) and
   press Enter; with --approve the script approves it through `graphicle call learning.review`.
4. Session 2 asks the question. It starts with the approved memory (the plugin's SessionStart hook) and should
   answer with the memory's id and the code's path:line.

Every session runs `muse exec --json` with Contributor mode off, within this kit's spending limit ($1 a run,
$10 a day, caps.json in all) and logged to its spend log, as muse_bench.py's runs are. OUT gets each session's events, the draft, and summary.json: the memory's
id, both answers, whether the second cites the memory, and each session's tokens and cost.
"""
import argparse
import json
import os
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import muse_bench  # noqa: E402

DECISION = (
    "Team decision for this repository: a new configuration field of a scheduler plugin is validated in "
    "pkg/scheduler/apis/config/validation (validation_pluginargs.go), never with ad hoc checks inside the "
    "plugin. Read that validation code with graphicle's tools, then record this decision as a project memory "
    "with graphicle's propose_memory tool: kind decision, a short title, the decision in one or two sentences, "
    "and as evidence one line you quote verbatim from the validation code. Reply with the memory's title.")
QUESTION = (
    "By this project's decisions, where should I validate a new configuration field of a scheduler plugin? "
    "Answer in two sentences, and cite the project memory by its id and the code by path:line.")


def graphicle(a, env, *args, stdin=None):
    out = subprocess.run([a.graphicle, "--data-dir", a.data, *args], cwd=a.repo, env=env, capture_output=True,
                         text=True, input=stdin)
    if out.returncode != 0:
        raise SystemExit(f"graphicle {' '.join(args)} failed: {out.stderr.strip()[:500]}")
    return out.stdout


def call(a, env, operation, arguments):
    with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as f:
        json.dump(arguments, f)
    try:
        return json.loads(graphicle(a, env, "call", operation, "--arguments", f.name))
    finally:
        os.unlink(f.name)


def memories(a, env):
    reply = call(a, env, "memories", {"include_retired": False})
    items = reply.get("memories", reply) if isinstance(reply, dict) else reply
    return [m for m in items if isinstance(m, dict)]


def session(a, env, name, prompt, price):
    """One capped `muse exec --json` session: its answer text, usage and cost."""
    day = muse_bench.now_iso()[:10]
    if muse_bench.logged(a.spend_log, "meta", muse_bench.KIT, day) + a.run_cap > a.day_cap:
        raise SystemExit("memory_demo: the day cap would be passed")
    total_cap = json.load(open(os.path.join(HERE, "caps.json")))["usd"]["meta"]
    if muse_bench.logged(a.spend_log, "meta") + a.run_cap > total_cap:
        raise SystemExit("memory_demo: the total cap (caps.json) would be passed")
    prompt_file = os.path.join(a.out, f"{name}.prompt.txt")
    with open(prompt_file, "w") as f:
        f.write(prompt)
    cmd = [a.muse, "exec", "--json", "--model", a.model, "--contributor-mode", "off", "--disable-approval",
           "--disable-request-user-input", "--trust-workspace", "--max-model-steps", "30",
           "--prompt-file", prompt_file]
    meter, reason, t0 = muse_bench.Meter(price), None, time.time()
    with open(os.path.join(a.out, f"{name}.events.jsonl"), "w") as raw, \
            open(os.path.join(a.out, f"{name}.stderr"), "w") as err:
        proc = subprocess.Popen(cmd, cwd=a.repo, env=env, stdout=subprocess.PIPE, stderr=err,
                                stdin=subprocess.DEVNULL, start_new_session=True, text=True)
        timer = threading.Timer(1200, lambda: muse_bench.stop(proc))
        timer.start()
        try:
            for line in proc.stdout:
                raw.write(line)
                try:
                    meter.feed(json.loads(line))
                except ValueError:
                    continue
                if reason is None and meter.spent() >= a.run_cap - 0.25:
                    reason = "run cap"
                    muse_bench.stop(proc)
            code = proc.wait()
        finally:
            timer.cancel()
    tin, cached, tout, treason = meter.usage()
    cost = meter.cost_of(tin, cached, tout)
    muse_bench.append_log(a.spend_log, {
        "time": muse_bench.now_iso(), "provider": "meta", "kit": muse_bench.KIT, "client": "muse-code",
        "model": a.model, "arm": f"memory-demo-{name}", "run": os.path.abspath(a.out),
        "status": "ok" if code == 0 and not reason else "stopped" if reason else "error",
        "input_tokens": tin, "cached_tokens": cached, "output_tokens": tout, "reasoning_tokens": treason,
        "model_calls": len(meter.calls), "seconds": round(time.time() - t0, 3), "cost_usd": round(cost, 6)})
    return {"answer": "\n".join(meter.texts), "exit_code": code, "stopped": reason, "model_calls": len(meter.calls),
            "tokens_in": tin, "tokens_cached": cached, "tokens_out": tout, "cost_usd": round(cost, 6)}


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--repo", required=True)
    ap.add_argument("--data", required=True, help="Graphicle's data folder for the demo")
    ap.add_argument("--out", required=True)
    ap.add_argument("--muse", default="muse")
    ap.add_argument("--graphicle", default="graphicle")
    ap.add_argument("--model", default="muse-spark-1.3")
    ap.add_argument("--key-file", default=None)
    ap.add_argument("--home", default=None, help="Muse Code's HOME for the demo (default OUT/home)")
    ap.add_argument("--approve", action="store_true", help="approve the draft through graphicle call")
    ap.add_argument("--spend-log", required=True, help="this kit's spend log, as muse_bench.py's")
    ap.add_argument("--run-cap", type=float, default=1.0)
    ap.add_argument("--day-cap", type=float, default=10.0)
    ap.add_argument("--decision", default=DECISION)
    ap.add_argument("--question", default=QUESTION)
    ap.add_argument("--prices", default=os.path.join(muse_bench.BENCH, "prices.json"))
    a = ap.parse_args()
    a.repo, a.data, a.out = (os.path.abspath(p) for p in (a.repo, a.data, a.out))
    a.home = os.path.abspath(a.home or os.path.join(a.out, "home"))
    os.makedirs(a.out, exist_ok=True)
    os.makedirs(a.home, exist_ok=True)
    key = open(a.key_file).read().strip() if a.key_file else os.environ.get("MODEL_API_KEY")
    if not key:
        raise SystemExit("memory_demo: no key: pass --key-file (a 0600 file) or set MODEL_API_KEY")
    price = json.load(open(a.prices))["models"][a.model]
    env = muse_bench.child_env(a.home, key)

    graphicle(a, env, "setup", "--yes", "--clients", "none", "--no-instructions", "--no-hooks", "--json")
    graphicle(a, env, "connect", "muse", "--hooks", "--allow-learning", "--json")
    subprocess.run([a.muse, "plugins", "approve", "graphicle", "--json"], env=env, check=True,
                   stdout=subprocess.DEVNULL)
    before = {m.get("id") for m in memories(a, env)}

    first = session(a, env, "session1", a.decision, price)
    print(f"Session 1 ({first['model_calls']} model calls, ${first['cost_usd']:.4f}):\n{first['answer']}\n")
    drafts = []
    for _ in range(60):
        drafts = [m for m in memories(a, env) if m.get("id") not in before and m.get("state") == "draft"]
        if drafts:
            break
        time.sleep(1)
    if not drafts:
        raise SystemExit("memory_demo: session 1 proposed no memory")
    draft = drafts[-1]
    with open(os.path.join(a.out, "draft.json"), "w") as f:
        json.dump(draft, f, indent=2)
    print(f"Draft memory {draft['id']}: {draft.get('title')}\n{draft.get('body', '')}\n")
    if a.approve:
        call(a, env, "learning.review", {"memory_id": draft["id"], "expected_revision": draft.get("revision", 1),
                                         "decision": "approve", "maintainer": "demo",
                                         "reason": "Checked the quoted validation code."})
    else:
        print(f"Approve it in the app: {a.graphicle} --data-dir {a.data} open (Memories, Review draft). "
              "Then press Enter.")
        sys.stdin.readline()
    approved = [m for m in memories(a, env) if m.get("id") == draft["id"]]
    if not approved or approved[0].get("state") != "reviewed":
        raise SystemExit(f"memory_demo: memory {draft['id']} is not approved")

    second = session(a, env, "session2", a.question, price)
    cited = draft["id"] in second["answer"]
    print(f"Session 2 ({second['model_calls']} model calls, ${second['cost_usd']:.4f}):\n{second['answer']}\n")
    print(f"Session 2 cites memory {draft['id']}: {'yes' if cited else 'no'}")
    with open(os.path.join(a.out, "summary.json"), "w") as f:
        json.dump({"memory_id": draft["id"], "title": draft.get("title"), "session1": first, "session2": second,
                   "cites_memory": cited, "model": a.model}, f, indent=2)


if __name__ == "__main__":
    main()
