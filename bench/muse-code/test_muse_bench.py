#!/usr/bin/env python3
"""Tests of muse_bench.py with a fake `muse` (and `graphicle`) program: scoring, usage, the plugin arm's
HOME, the spend log, and the caps that stop a run. No model is called.

usage: python3 -m unittest -v test_muse_bench
"""
import json
import os
import stat
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import muse_bench  # noqa: E402

# A fake muse: `plugins …` records its call; `exec --json …` prints the events of the mode in
# FAKE_MODE (a file beside the program), reading the prompt file it is given.
FAKE_MUSE = r'''#!/usr/bin/env python3
import json, os, sys, time
here = os.path.dirname(os.path.abspath(__file__))
with open(os.path.join(here, "calls.log"), "a") as f:
    f.write(json.dumps({"args": sys.argv[1:], "cwd": os.getcwd(), "home": os.environ.get("HOME"),
                        "key": "META_API_KEY" in os.environ}) + "\n")
if sys.argv[1] == "plugins":
    print(json.dumps({"ok": True}))
    sys.exit(0)
mode = open(os.path.join(here, "FAKE_MODE")).read().strip()
prompt = open(sys.argv[sys.argv.index("--prompt-file") + 1]).read()
assert "List the source files" in prompt
def emit(event):
    print(json.dumps(event), flush=True)
if mode == "answer":
    emit({"type": "run.started", "run_id": "r1"})
    emit({"type": "agent.event", "payload": {"kind": "run", "event": {"kind": "model_completed",
          "response_id": "a", "usage": {"input_tokens": 20000, "cached_tokens": 0, "output_tokens": 300}}}})
    emit({"type": "agent.event", "payload": {"kind": "run", "event": {"kind": "model_completed",
          "response_id": "b", "usage": {"input_tokens": 30000, "cached_tokens": 19000, "output_tokens": 200}}}})
    emit({"type": "agent.event", "payload": {"kind": "run", "event": {"kind": "assistant_message_committed",
          "text": "{\"files\": [\"./pkg/a.go\", \"pkg/b.go:12\", \"missing.go\"]}"}}})
    emit({"type": "run.usage", "models": [{"model": "muse-spark-1.3", "input_tokens": 50000,
          "cached_input_tokens": 19000, "output_tokens": 500, "reasoning_tokens": 0}],
          "total": {"input_tokens": 50000, "cached_input_tokens": 19000, "output_tokens": 500,
                    "reasoning_tokens": 0}})
elif mode == "spend":
    for i in range(1000):
        emit({"type": "agent.event", "payload": {"kind": "run", "event": {"kind": "model_completed",
              "response_id": f"c{i}", "usage": {"input_tokens": 200000, "cached_tokens": 0,
                                                 "output_tokens": 1000}}}})
        time.sleep(0.05)
'''

# A fake graphicle: `connect` writes the plugin's manifest where `connect muse --hooks` would, with six
# hooks; `setup` (the sync) records only that it ran.
FAKE_GRAPHICLE = r'''#!/usr/bin/env python3
import json, os, sys
data = sys.argv[sys.argv.index("--data-dir") + 1]
if "connect" not in sys.argv:
    print("{}")
    sys.exit(0)
assert sys.argv[sys.argv.index("connect"):][:3] == ["connect", "muse", "--hooks"]
folder = os.path.join(data, "muse-code-plugin", ".muse-plugin")
os.makedirs(folder, exist_ok=True)
hooks = [{"id": h, "event": e, "command": ["graphicle", "hook", "muse", h], "timeoutMs": 1000}
         for h, e in [("session-start", "SessionStart"), ("prompt-context", "UserPromptSubmit"),
                      ("tool-batch", "PostToolBatch"), ("stop", "Stop"), ("pre-compact", "PreCompact"),
                      ("session-end", "SessionEnd")]]
json.dump({"name": "graphicle", "capabilities": {"hooks": hooks}}, open(os.path.join(folder, "plugin.json"), "w"))
print("{}")
'''


def program(path, text):
    with open(path, "w") as f:
        f.write(text)
    os.chmod(path, os.stat(path).st_mode | stat.S_IXUSR)


class MuseBenchTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = self.tmp.name
        self.work, self.bin = os.path.join(root, "work"), os.path.join(root, "bin")
        src = os.path.join(self.work, "src", "demo")
        os.makedirs(os.path.join(src, "pkg"))
        os.makedirs(self.bin)
        for name in ("a.go", "b.go"):
            with open(os.path.join(src, "pkg", name), "w") as f:
                f.write("package pkg\n")
        env = dict(os.environ, GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@example.com", GIT_COMMITTER_NAME="t",
                   GIT_COMMITTER_EMAIL="t@example.com", GIT_CONFIG_NOSYSTEM="1")
        for args in (["init", "-q"], ["add", "."], ["commit", "-q", "-m", "s"]):
            subprocess.run(["git", *args], cwd=src, check=True, env=env)
        self.sha = subprocess.run(["git", "rev-parse", "HEAD"], cwd=src, capture_output=True, text=True,
                                  check=True).stdout.strip()
        self.questions = os.path.join(root, "questions")
        self.texts = os.path.join(root, "texts")
        os.makedirs(self.questions)
        os.makedirs(self.texts)
        with open(os.path.join(self.questions, "questions-demo.jsonl"), "w") as f:
            for n in (1, 2):
                f.write(json.dumps({"id": f"demo-{n}", "repo": "o/demo", "S": self.sha,
                                    "gold": ["pkg/b.go"]}) + "\n")
        with open(os.path.join(self.texts, "texts-demo.jsonl"), "w") as f:
            for n in (1, 2):
                f.write(json.dumps({"id": f"demo-{n}", "text": f"Issue {n}: b is wrong."}) + "\n")
        self.key = os.path.join(root, "key")
        with open(self.key, "w") as f:
            f.write("test-key-not-real\n")
        program(os.path.join(self.bin, "muse"), FAKE_MUSE)
        program(os.path.join(self.bin, "graphicle"), FAKE_GRAPHICLE)
        self.log = os.path.join(root, "spend", "spend-log.jsonl")

    def tearDown(self):
        self.tmp.cleanup()

    def mode(self, mode):
        with open(os.path.join(self.bin, "FAKE_MODE"), "w") as f:
            f.write(mode)

    def run_bench(self, arm, *extra):
        out = os.path.join(self.tmp.name, "out", arm)
        cmd = [sys.executable, os.path.join(HERE, "muse_bench.py"), "--arm", arm, "--work", self.work,
               "--texts", self.texts, "--out", out, "--questions", self.questions, "--repos", "demo",
               "--muse", os.path.join(self.bin, "muse"), "--graphicle", os.path.join(self.bin, "graphicle"),
               "--key-file", self.key, "--spend-log", self.log, *extra]
        done = subprocess.run(cmd, capture_output=True, text=True, timeout=300)
        rows_path = os.path.join(out, "rows.jsonl")
        rows = []
        if os.path.exists(rows_path):
            with open(rows_path) as f:
                rows = [json.loads(l) for l in f if l.strip()]
        return done, rows

    def calls(self):
        with open(os.path.join(self.bin, "calls.log")) as f:
            return [json.loads(l) for l in f]

    def test_scores_usage_and_log(self):
        self.mode("answer")
        done, rows = self.run_bench("plain")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual([r["id"] for r in rows], ["demo-1", "demo-2"])
        row = rows[0]
        self.assertEqual(row["files"], ["pkg/a.go", "pkg/b.go", "missing.go"])
        self.assertEqual((row["hit1"], row["hit3"], row["rr"]), (0, 1, 0.5))
        # The run.usage total wins over the per-call sum.
        self.assertEqual((row["tokens_in"], row["tokens_cached"], row["tokens_out"]), (50000, 19000, 500))
        self.assertEqual(row["model_calls"], 2)
        expected = ((50000 - 19000) * 1.25 + 19000 * 0.15 + 500 * 4.25) / 1e6
        self.assertAlmostEqual(row["cost_usd"], expected, places=6)
        with open(self.log) as f:
            text = f.read()
        lines = [json.loads(l) for l in text.splitlines() if l.strip()]
        self.assertEqual([l["kit"] for l in lines], ["muse-code", "muse-code"])
        self.assertEqual(lines[0]["provider"], "meta")
        self.assertNotIn("test-key", text)
        execs = [c for c in self.calls() if c["args"][0] == "exec"]
        self.assertTrue(all(c["key"] for c in execs))
        for flag in ("--json", "--disable-approval", "--trust-workspace"):
            self.assertIn(flag, execs[0]["args"])
        self.assertEqual(execs[0]["args"][execs[0]["args"].index("--contributor-mode") + 1], "off")
        # Each question gets a fresh HOME.
        self.assertNotEqual(execs[0]["home"], execs[1]["home"])
        # Resumed: questions in rows.jsonl are skipped.
        again, rows = self.run_bench("plain")
        self.assertEqual(len(rows), 2)

    def test_plugin_home_drops_capture_hooks(self):
        self.mode("answer")
        done, rows = self.run_bench("plugin", "--first", "1")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(len(rows), 1)
        plugin_calls = [c["args"][:3] for c in self.calls() if c["args"][0] == "plugins"]
        self.assertEqual(plugin_calls, [["plugins", "update", "graphicle"], ["plugins", "approve", "graphicle"]])
        state = os.path.join(self.tmp.name, "out", "state")
        with open(os.path.join(state, "graphicle-data", "muse-code-plugin", ".muse-plugin", "plugin.json")) as f:
            manifest = json.load(f)
        self.assertEqual([h["id"] for h in manifest["capabilities"]["hooks"]],
                         ["session-start", "prompt-context", "tool-batch"])

    def test_run_cap_stops_a_run(self):
        self.mode("spend")
        done, rows = self.run_bench("plain", "--first", "1", "--run-cap", "1.0", "--margin", "0.25")
        self.assertEqual(done.returncode, 0, done.stderr)
        row = rows[0]
        self.assertTrue(row["stopped"].startswith("run cap"), row)
        # Stopped at the first call past $0.75: each call costs $0.25425.
        self.assertLess(row["cost_usd"], 1.1)
        self.assertGreaterEqual(row["cost_usd"], 0.75)

    def test_day_cap_refuses_a_run(self):
        self.mode("answer")
        os.makedirs(os.path.dirname(self.log))
        with open(self.log, "w") as f:
            f.write(json.dumps({"time": muse_bench.now_iso(), "provider": "meta", "kit": "muse-code",
                                "cost_usd": 9.5}) + "\n")
        done, rows = self.run_bench("plain")
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("day cap", done.stderr)
        self.assertEqual(rows, [])


if __name__ == "__main__":
    unittest.main()
