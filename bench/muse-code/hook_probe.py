#!/usr/bin/env python3
"""Probes of Muse Code's plugins, in short headless sessions: which hook events add context to the model's
input, and what Muse Code does when a plugin's MCP server cannot start.

usage: hook_probe.py --out DIR --spend-log FILE [--muse muse] [--model muse-spark-1.3] [--key-file FILE]
         [--run-cap 0.25] [--server-cap 0.10]

1. Hook events. A probe plugin (OUT/probe-plugin) has one hook per candidate event: SessionStart,
   UserPromptSubmit, PreLLMCall, PostToolUse and PostToolBatch. Each prints `additionalContext` with its own
   marker, `PROBE-<event>-<nonce>`, in the shape Muse Code's hook output takes
   (`{"hookSpecificOutput":{"hookEventName":…,"additionalContext":…}}`), and appends its input to
   `${PLUGIN_DATA}/hook-inputs.jsonl`, so the payload of each event is on record too. In an isolated HOME the
   plugin is validated, installed and approved; one `muse exec --json` session runs a prompt that makes one
   shell call and asks the model to repeat every PROBE- line it can see. Three readings per event, in
   OUT/probe.json:
   - `fired`: the hook ran (its input was recorded);
   - `in_log`: its marker is in Muse Code's session log (session.jsonl), where the conversation is recorded;
   - `seen_by_model`: the model repeated the marker in its answer.
   The order of the markers in the log is recorded too, so a reader can tell whether context from a later
   event changes the start of the conversation. The plugin is removed afterwards.
2. A server that cannot start. Two more plugins, installed one at a time, each with one MCP server whose
   program exits at once with status 1: one sets `"mode": "optional"`, the other sets no mode. For each, one
   session asks for the word OK with one model step. OUT/probe.json's `failing_server` records, per plugin,
   the exit status, whether the session answered, and the end of its error output: whether Muse Code skips
   such a server or stops the session, and whether the field changes it.

Every session is capped and logged like muse_bench.py's runs, within this kit's spending limit: $0.25 for the
hook session and $0.10 for each server session by default.
"""
import argparse
import json
import os
import secrets
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import muse_bench  # noqa: E402

EVENTS = ["SessionStart", "UserPromptSubmit", "PreLLMCall", "PostToolUse", "PostToolBatch"]
PROBE = r'''#!/usr/bin/env python3
import json, os, sys
event, nonce, record = sys.argv[1], sys.argv[2], sys.argv[3]
data = sys.stdin.read()
os.makedirs(os.path.dirname(record) or ".", exist_ok=True)
with open(record, "a") as f:
    f.write(json.dumps({"event": event, "input": data[:20000]}) + "\n")
print(json.dumps({"hookSpecificOutput": {"hookEventName": event,
                  "additionalContext": f"PROBE-{event}-{nonce}"}}))
'''
FAILING_SERVER = "import sys\nsys.stderr.write('probe server: exits at once\\n')\nsys.exit(1)\n"
PROMPT = ("Run the shell command `ls` once. Then list, one per line and exactly as written, every line in your "
          "context that starts with PROBE-. If you see none, say NONE.")
SERVER_PROMPT = "Reply with the single word OK."


def manifest(name, description, hooks=(), servers=()):
    return {"schemaVersion": 1, "name": name, "displayName": name, "version": "0.1.0",
            "description": description, "compat": {"source": "native", "manifestDir": ".muse-plugin"},
            "capabilities": {"skills": [], "commands": [], "hooks": list(hooks), "mcpServers": list(servers),
                             "reminders": []}}


def write_plugin(folder, manifest_value, files):
    os.makedirs(os.path.join(folder, ".muse-plugin"), exist_ok=True)
    for relative, text in files.items():
        path = os.path.join(folder, relative)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as f:
            f.write(text)
    with open(os.path.join(folder, ".muse-plugin", "plugin.json"), "w") as f:
        json.dump(manifest_value, f, indent=2)


def muse(a, env, *args):
    done = subprocess.run([a.muse, *args], env=env, capture_output=True, text=True)
    return {"exit": done.returncode, "stdout": done.stdout[-4000:], "stderr": done.stderr[-2000:]}


def install(a, env, folder, name, steps):
    """Validate, install and approve a plugin; the steps' outputs go in `steps`."""
    for step, args in [("validate", ["plugins", "validate", folder, "--json"]),
                       ("install", ["plugins", "install", folder, "--scope", "user", "--json"]),
                       ("approve", ["plugins", "approve", name, "--json"])]:
        steps[f"{name}:{step}"] = muse(a, env, *args)
        if steps[f"{name}:{step}"]["exit"] != 0:
            return False
    return True


def session(a, env, work, name, prompt, max_steps, cap, price):
    """One capped `muse exec --json` session, logged to the spend log."""
    prompt_file = os.path.join(a.out, f"{name}.prompt.txt")
    with open(prompt_file, "w") as f:
        f.write(prompt)
    cmd = [a.muse, "exec", "--json", "--model", a.model, "--contributor-mode", "off", "--disable-approval",
           "--disable-request-user-input", "--trust-workspace", "--max-model-steps", str(max_steps),
           "--prompt-file", prompt_file]
    meter, reason, t0 = muse_bench.Meter(price), None, time.time()
    err_path = os.path.join(a.out, f"{name}.stderr.txt")
    with open(os.path.join(a.out, f"{name}.events.jsonl"), "w") as raw, open(err_path, "w") as err:
        proc = subprocess.Popen(cmd, cwd=work, env=env, stdout=subprocess.PIPE, stderr=err,
                                stdin=subprocess.DEVNULL, start_new_session=True, text=True)
        timer = threading.Timer(600, lambda: muse_bench.stop(proc))
        timer.start()
        try:
            for line in proc.stdout:
                raw.write(line)
                try:
                    meter.feed(json.loads(line))
                except ValueError:
                    continue
                if reason is None and meter.spent() >= cap:
                    reason = "run cap"
                    muse_bench.stop(proc)
            code = proc.wait()
        finally:
            timer.cancel()
    tin, cached, tout, treason = meter.usage()
    cost = meter.cost_of(tin, cached, tout)
    muse_bench.append_log(a.spend_log, {
        "time": muse_bench.now_iso(), "provider": "meta", "kit": muse_bench.KIT, "client": "muse-code",
        "model": a.model, "arm": f"probe-{name}", "run": a.out, "status": "ok" if code == 0 and not reason else
        "stopped" if reason else "error", "input_tokens": tin, "cached_tokens": cached, "output_tokens": tout,
        "reasoning_tokens": treason, "model_calls": len(meter.calls), "seconds": round(time.time() - t0, 3),
        "cost_usd": round(cost, 6)})
    with open(err_path, encoding="utf-8", errors="replace") as f:
        stderr_tail = f.read()[-2000:]
    return {"exit_code": code, "stopped": reason, "model_calls": len(meter.calls), "cost_usd": round(cost, 6),
            "answer": "\n".join(meter.texts)[-4000:], "stderr_tail": stderr_tail}


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--out", required=True)
    ap.add_argument("--spend-log", required=True)
    ap.add_argument("--muse", default="muse")
    ap.add_argument("--model", default="muse-spark-1.3")
    ap.add_argument("--key-file", default=None)
    ap.add_argument("--run-cap", type=float, default=0.25, help="the hook session's cap")
    ap.add_argument("--server-cap", type=float, default=0.10, help="each server session's cap")
    ap.add_argument("--prices", default=os.path.join(muse_bench.BENCH, "prices.json"))
    a = ap.parse_args()
    a.out = os.path.abspath(a.out)
    os.makedirs(a.out, exist_ok=True)
    key = open(a.key_file).read().strip() if a.key_file else os.environ.get("MODEL_API_KEY")
    if not key:
        raise SystemExit("hook_probe: no key: pass --key-file (a 0600 file) or set MODEL_API_KEY")
    total = a.run_cap + 2 * a.server_cap
    if muse_bench.logged(a.spend_log, "meta", muse_bench.KIT, muse_bench.now_iso()[:10]) + total > 10.0:
        raise SystemExit("hook_probe: the day cap would be passed")
    price = json.load(open(a.prices))["models"][a.model]
    home = os.path.join(a.out, "home")
    work = os.path.join(a.out, "workspace")
    os.makedirs(work, exist_ok=True)
    with open(os.path.join(work, "README.md"), "w") as f:
        f.write("# Probe workspace\n")
    env = muse_bench.child_env(home, key)
    steps = {}
    result = {"model": a.model, "steps": steps}

    # 1. Hook events.
    nonce = secrets.token_hex(4)
    hooks, files = [], {}
    for event in EVENTS:
        script = f"hooks/{event.lower()}.py"
        files[script] = PROBE
        hooks.append({"id": event.lower(), "event": event,
                      "command": ["python3", script, event, nonce, "${PLUGIN_DATA}/hook-inputs.jsonl"],
                      "timeoutMs": 5000})
    plugin = os.path.join(a.out, "probe-plugin")
    write_plugin(plugin, manifest("hook-probe", "Adds a marker as context on each hook event.", hooks=hooks), files)
    result["nonce"] = nonce
    if install(a, env, plugin, "hook-probe", steps):
        hooks_run = session(a, env, work, "hooks", PROMPT, 6, a.run_cap, price)
        steps["hook-probe:remove"] = muse(a, env, "plugins", "remove", "hook-probe", "--json")
        # The hooks' record, wherever Muse Code put the plugin's data (or the workspace, unsubstituted).
        inputs = []
        for top in (home, work):
            for folder, _dirs, names in os.walk(top):
                if "hook-inputs.jsonl" in names:
                    with open(os.path.join(folder, "hook-inputs.jsonl")) as f:
                        inputs.extend(json.loads(line) for line in f if line.strip())
        fired = {item["event"] for item in inputs}
        with open(os.path.join(a.out, "hook-inputs.jsonl"), "w") as f:
            for item in inputs:
                f.write(json.dumps(item) + "\n")
        logs = []
        for folder, _dirs, names in os.walk(os.path.join(home, ".local/share/muse/sessions")):
            if "session.jsonl" in names and "subagent" not in folder:
                logs.append(os.path.join(folder, "session.jsonl"))
        log_text = "".join(open(p, encoding="utf-8", errors="replace").read() for p in sorted(logs))
        markers = {e: f"PROBE-{e}-{nonce}" for e in EVENTS}
        order = sorted((log_text.find(m), e) for e, m in markers.items() if m in log_text)
        result["hooks"] = dict(hooks_run, session_logs=logs, first_offset_in_log=[e for _, e in order])
        result["events"] = {e: {"fired": e in fired, "in_log": markers[e] in log_text,
                                "seen_by_model": markers[e] in hooks_run["answer"]} for e in EVENTS}

    # 2. A server that cannot start, with and without "mode": "optional".
    result["failing_server"] = {}
    for mode in ("optional", None):
        label = mode or "default"
        name = f"server-probe-{label}"
        server = {"id": "failing", "transport": "stdio", "command": ["python3", "mcp/fail.py"]}
        if mode:
            server["mode"] = mode
        folder = os.path.join(a.out, name)
        write_plugin(folder, manifest(name, "One MCP server that cannot start.", servers=[server]),
                     {"mcp/fail.py": FAILING_SERVER})
        if not install(a, env, folder, name, steps):
            result["failing_server"][label] = {"installed": False}
            continue
        run = session(a, env, work, name, SERVER_PROMPT, 1, a.server_cap, price)
        steps[f"{name}:remove"] = muse(a, env, "plugins", "remove", name, "--json")
        result["failing_server"][label] = dict(run, installed=True, answered=bool(run["answer"].strip()))

    with open(os.path.join(a.out, "probe.json"), "w") as f:
        json.dump(result, f, indent=2)
    for e, r in result.get("events", {}).items():
        print(f"{e:18} fired={r['fired']!s:5} in_log={r['in_log']!s:5} seen_by_model={r['seen_by_model']}")
    for label, r in result["failing_server"].items():
        print(f"failing server ({label}): installed={r.get('installed')} exit={r.get('exit_code')} "
              f"answered={r.get('answered')}")
    print(f"details in {a.out}/probe.json")


if __name__ == "__main__":
    main()
