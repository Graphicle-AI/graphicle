#!/usr/bin/env python3
"""Tests of the cache meter's session (c4c.py run) on the mock server: no model, no key, no spend.

usage: test_c4c.py [-v]   (MOCK_HOST as in ../whole-repo/test_runner.py)
Writes 20 synthetic packs (a shared 90% prefix and a question's own suffix, or no shared text at all), their
manifest and questions, serves mock_server.py in a thread and runs the session.
"""
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
WR = os.path.join(HERE, "..", "whole-repo")
sys.path.insert(0, WR)
import mock_server  # noqa: E402
from test_runner import mock_host, read_jsonl  # noqa: E402

PREFIX = "".join(f'<file path="pkg/kubelet/f{i}.go" lines="1-1">\nfunc F{i}() {{ return }}\n</file>\n' for i in range(900))


def make_session(root, shared):
    work = os.path.join(root, "work")
    packs = os.path.join(work, "c4c", "packs")
    os.makedirs(packs)
    qfile, tdir = os.path.join(root, "questions-kubernetes.jsonl"), os.path.join(root, "texts")
    os.makedirs(tdir)
    manifest = {"graphicle": "test", "S": "d5cbb79be916" + "0" * 28, "budget": 1015808, "layout": "cache",
                "prefix_share": 0.9, "counter": "estimate", "packs": [], "one_prefix": True}
    with open(qfile, "w") as qf, open(os.path.join(tdir, "texts-kubernetes.jsonl"), "w") as tf:
        for i in range(1, 21):
            qid, text = f"kubernetes-{i}", f"kubelet handler {i} fails"
            qf.write(json.dumps({"id": qid, "repo": "kubernetes/kubernetes", "S": manifest["S"],
                                 "gold": [f"pkg/kubelet/f{i}.go"], "names_gold_path": False,
                                 "text_sha256": hashlib.sha256(text.encode()).hexdigest()}) + "\n")
            tf.write(json.dumps({"id": qid, "text": text}) + "\n")
            body = (PREFIX if shared else PREFIX.replace("func", f"func{i}x")) + \
                f'<file path="pkg/kubelet/f{i}.go" lines="1-1">\nfunc Q{i}() {{}}\n</file>\n'
            with open(os.path.join(packs, qid + ".xml"), "w") as f:
                f.write(body)
            manifest["packs"].append({"id": qid, "tokens": len(body) // 4, "prefix_sha256": "ab" * 32,
                                      "xml_sha256": hashlib.sha256(body.encode()).hexdigest()})
    with open(os.path.join(work, "c4c", "packs.json"), "w") as f:
        json.dump(manifest, f)
    return work, qfile, tdir


class C4cTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="c4c-test-")
        cls.record = os.path.join(cls.tmp, "mock-requests.jsonl")
        cls.srv = mock_server.make_server(mock_host(), 0, record=cls.record)
        threading.Thread(target=cls.srv.serve_forever, daemon=True).start()
        cls.url = f"http://{cls.srv.server_address[0]}:{cls.srv.server_address[1]}/v1"
        cls.caps = os.path.join(cls.tmp, "caps.json")
        with open(cls.caps, "w") as f:
            json.dump({"usd": {"mock": 50.0}}, f)

    @classmethod
    def tearDownClass(cls):
        cls.srv.shutdown()
        cls.srv.server_close()
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def session(self, name, shared):
        root = os.path.join(self.tmp, name)
        work, qfile, tdir = make_session(root, shared)
        out = os.path.join(root, "out")
        r = subprocess.run([sys.executable, os.path.join(HERE, "c4c.py"), "run", "--work", work, "--out", out,
                            "--texts", tdir, "--questions", qfile, "--profile", "mock", "--base-url", self.url,
                            "--caps", self.caps, "--spend-log", os.path.join(root, "spend-log.jsonl")],
                           capture_output=True, text=True, timeout=600, env=dict(os.environ, MODEL_API_KEY="mock-key"))
        self.assertEqual(r.returncode, 0, r.stderr[-2000:] + r.stdout[-1000:])
        calls_path = os.path.join(out, "calls.jsonl")
        self.assertTrue(os.path.exists(calls_path), r.stdout[-1500:])
        return read_jsonl(calls_path), json.load(open(os.path.join(out, "summary.json"))), \
            read_jsonl(os.path.join(root, "spend-log.jsonl"))

    def test_1_follow_ups_read_the_shared_prefix(self):
        calls, summary, log = self.session("shared", True)
        self.assertEqual([c["model"] for c in calls], ["mock"] * 20 + ["mock-wonder"] * 10)
        self.assertEqual(calls[0]["cached_tokens"], 0, "the first call pays in full")
        self.assertTrue(all(c["cached_share"] > 0.8 for c in calls[1:20]), [c["cached_share"] for c in calls[:20]])
        self.assertEqual(len({c["cache_key"] for c in calls}), 1)
        self.assertIsNone(summary["stopped"])
        fu = summary["mock"]["follow_ups"]
        self.assertEqual(fu["n"], 19)
        self.assertLess(fu["cost_mean"], summary["mock"]["first"]["cost_usd"])
        self.assertEqual(len(log), 31, "30 calls and the window check, all in the ledger")
        self.assertEqual(log[0]["api"], "input_tokens")

    def test_2_the_session_stops_when_the_cache_does_not_serve(self):
        calls, summary, _log = self.session("unshared", False)
        self.assertEqual(len(calls), 3)
        self.assertIn("less than half", summary["stopped"])


if __name__ == "__main__":
    unittest.main()
