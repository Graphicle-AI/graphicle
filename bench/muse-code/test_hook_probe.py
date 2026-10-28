#!/usr/bin/env python3
"""Tests of hook_probe.py with a fake `muse` program: the hook readings (fired, in the session log, seen by
the model), the failing-server sessions with and without "mode": "optional", the plugins' removal, and the
spend log. No model is called.

usage: python3 -m unittest -v test_hook_probe
"""
import json
import os
import stat
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))

# The fake keeps the installed plugins in a state file beside it. `exec` with the hook probe installed runs
# its SessionStart hook (with ${PLUGIN_DATA} under HOME), writes a session log holding that hook's marker,
# and answers with the marker; with a failing server installed it answers OK when the server is optional
# and stops with status 1 otherwise, as a required server would.
FAKE_MUSE = r'''#!/usr/bin/env python3
import json, os, subprocess, sys
here = os.path.dirname(os.path.abspath(__file__))
state_path = os.path.join(here, "installed.json")
installed = json.load(open(state_path)) if os.path.exists(state_path) else {}
args = sys.argv[1:]
with open(os.path.join(here, "calls.log"), "a") as f:
    f.write(" ".join(args[:3]) + "\n")
def emit(event):
    print(json.dumps(event), flush=True)
if args[0] == "plugins":
    if args[1] == "install":
        manifest = json.load(open(os.path.join(args[2], ".muse-plugin", "plugin.json")))
        installed[manifest["name"]] = args[2]
    elif args[1] == "remove":
        installed.pop(args[2], None)
    json.dump(installed, open(state_path, "w"))
    print(json.dumps({"ok": True}))
    sys.exit(0)
home = os.environ["HOME"]
emit({"type": "agent.event", "payload": {"kind": "run", "event": {"kind": "model_completed",
      "response_id": "r1", "usage": {"input_tokens": 1000, "cached_tokens": 0, "output_tokens": 10}}}})
if "hook-probe" in installed:
    folder = installed["hook-probe"]
    manifest = json.load(open(os.path.join(folder, ".muse-plugin", "plugin.json")))
    hook = next(h for h in manifest["capabilities"]["hooks"] if h["event"] == "SessionStart")
    data = os.path.join(home, ".local/share/muse/plugin-data/hook-probe")
    argv = [part.replace("${PLUGIN_DATA}", data) for part in hook["command"]]
    out = subprocess.run(argv, cwd=folder, input=json.dumps({"hook_event_name": "SessionStart"}),
                         capture_output=True, text=True, check=True).stdout
    marker = json.loads(out)["hookSpecificOutput"]["additionalContext"]
    log = os.path.join(home, ".local/share/muse/sessions/2026/10/27/s1/session.jsonl")
    os.makedirs(os.path.dirname(log), exist_ok=True)
    with open(log, "w") as f:
        f.write(json.dumps({"payload": {"event": {"kind": "hook_context", "text": marker}}}) + "\n")
    emit({"type": "agent.event", "payload": {"kind": "run", "event": {"kind": "assistant_message_committed",
          "text": marker}}})
    sys.exit(0)
failing = [n for n in installed if n.startswith("server-probe-")]
if failing:
    manifest = json.load(open(os.path.join(installed[failing[0]], ".muse-plugin", "plugin.json")))
    if manifest["capabilities"]["mcpServers"][0].get("mode") != "optional":
        sys.stderr.write("MCP server failing (required) did not start\n")
        sys.exit(1)
emit({"type": "agent.event", "payload": {"kind": "run", "event": {"kind": "assistant_message_committed",
      "text": "OK"}}})
'''


class HookProbeTest(unittest.TestCase):
    def test_readings_failing_servers_and_log(self):
        with tempfile.TemporaryDirectory() as root:
            program = os.path.join(root, "bin", "muse")
            os.makedirs(os.path.dirname(program))
            with open(program, "w") as f:
                f.write(FAKE_MUSE)
            os.chmod(program, os.stat(program).st_mode | stat.S_IXUSR)
            key = os.path.join(root, "key")
            with open(key, "w") as f:
                f.write("test-key-not-real\n")
            out, log = os.path.join(root, "out"), os.path.join(root, "spend.jsonl")
            done = subprocess.run([sys.executable, os.path.join(HERE, "hook_probe.py"), "--out", out,
                                   "--spend-log", log, "--muse", program, "--key-file", key],
                                  capture_output=True, text=True, timeout=300)
            self.assertEqual(done.returncode, 0, done.stderr)
            with open(os.path.join(out, "probe.json")) as f:
                result = json.load(f)
            start = result["events"]["SessionStart"]
            self.assertEqual((start["fired"], start["in_log"], start["seen_by_model"]), (True, True, True))
            self.assertFalse(result["events"]["PreLLMCall"]["fired"])
            self.assertEqual(result["hooks"]["first_offset_in_log"], ["SessionStart"])
            optional, default = result["failing_server"]["optional"], result["failing_server"]["default"]
            self.assertEqual((optional["exit_code"], optional["answered"]), (0, True))
            self.assertEqual((default["exit_code"], default["answered"]), (1, False))
            self.assertIn("did not start", default["stderr_tail"])
            with open(os.path.join(root, "bin", "installed.json")) as f:
                self.assertEqual(json.load(f), {}, "every probe plugin is removed")
            with open(log) as f:
                lines = [json.loads(line) for line in f if line.strip()]
            self.assertEqual([l["arm"] for l in lines],
                             ["probe-hooks", "probe-server-probe-optional", "probe-server-probe-default"])
            self.assertTrue(all(l["kit"] == "muse-code" for l in lines))
            with open(log) as f:
                self.assertNotIn("test-key", f.read())


if __name__ == "__main__":
    unittest.main()
