#!/usr/bin/env python3
"""Tests of e2.py on the mock server: no model, no key, no spend.

usage: test_e2.py [-v]   (MOCK_HOST as in ../whole-repo/test_runner.py)
Builds a work folder with two synthetic tasks (a bare repository as the mirror, problem statements, the
contexts E1 would save and their records), serves mock_server.py in a thread and runs e2.py per arm.
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
from test_runner import make_repo, mock_host, read_jsonl  # noqa: E402

FILES = {"pkg/cart/total.go": "package cart\n// Total sums the cart lines with tax.\nfunc Total() {}\n",
         "pkg/cart/tax.go": "package cart\nfunc Tax() {}\n", "pkg/web/routes.go": "package web\nfunc Routes() {}\n"}


class E2Test(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="e2-test-")
        work = cls.work = os.path.join(cls.tmp, "work")
        sha = make_repo(os.path.join(cls.tmp, "upstream"), FILES)
        os.makedirs(os.path.join(work, "mirror"))
        subprocess.run(["git", "clone", "-q", "--bare", os.path.join(cls.tmp, "upstream"),
                        os.path.join(work, "mirror", "acme__cart.git")], check=True)
        tasks, ids = [], []
        for i, (gold, text) in enumerate([("pkg/cart/total.go", "Cart total ignores the tax"),
                                          ("pkg/web/routes.go", "Routes are registered twice")], 1):
            iid = f"instance_acme__cart-{i}"
            ids.append(iid)
            tasks.append({"instance_id": iid, "repo": "acme/cart", "base_commit": sha, "language": "go", "gold": [gold],
                          "added": [], "text_sha256": hashlib.sha256(text.encode()).hexdigest(),
                          "names_gold_path": False, "hard": False})
            os.makedirs(os.path.join(work, "texts"), exist_ok=True)
            with open(os.path.join(work, "texts", iid + ".txt"), "w") as f:
                f.write(text)
            ctx_dir = os.path.join(work, "contexts", iid)
            os.makedirs(ctx_dir)
            rec = {}
            for fname in ("pack-64K.xml", "bm25-64K.txt"):
                body = "".join(f'<file path="{p}" lines="1-3">\n{t}</file>\n' for p, t in FILES.items())
                with open(os.path.join(ctx_dir, fname), "w") as f:
                    f.write(body)
                rec[fname] = {"sha256": hashlib.sha256(body.encode()).hexdigest(), "tokens": len(body) // 4, "files": 3}
            os.makedirs(os.path.join(work, "out", "e1", "tasks"), exist_ok=True)
            with open(os.path.join(work, "out", "e1", "tasks", iid + ".json"), "w") as f:
                json.dump({"id": iid, "repo": "acme/cart", "contexts": rec}, f)
        # A third task on which E1 failed: no saved contexts, only a .failed record. Every arm leaves it out.
        tasks.append(dict(tasks[0], instance_id="instance_acme__cart-3"))
        ids.append("instance_acme__cart-3")
        with open(os.path.join(work, "out", "e1", "tasks", "instance_acme__cart-3.json.failed"), "w") as f:
            json.dump({"id": "instance_acme__cart-3", "error": "RuntimeError: graphicle pack: ..."}, f)
        with open(os.path.join(work, "tasks.jsonl"), "w") as f:
            f.write("".join(json.dumps(t) + "\n" for t in tasks))
        cls.ids = os.path.join(cls.tmp, "sample.json")
        with open(cls.ids, "w") as f:
            json.dump({"ids": ids}, f)
        cls.record = os.path.join(cls.tmp, "mock-requests.jsonl")
        cls.srv = mock_server.make_server(mock_host(), 0, record=cls.record)
        threading.Thread(target=cls.srv.serve_forever, daemon=True).start()
        cls.url = f"http://{cls.srv.server_address[0]}:{cls.srv.server_address[1]}/v1"
        cls.caps = os.path.join(cls.tmp, "caps.json")
        with open(cls.caps, "w") as f:
            json.dump({"usd": {"mock": 5.0}}, f)
        cls.log = os.path.join(cls.tmp, "spend", "spend-log.jsonl")

    @classmethod
    def tearDownClass(cls):
        cls.srv.shutdown()
        cls.srv.server_close()
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def e2(self, arm, out, *extra, caps=None):
        return subprocess.run([sys.executable, os.path.join(HERE, "e2.py"), "--work", self.work, "--arm", arm,
                               "--out", os.path.join(self.tmp, out), "--ids", self.ids, "--profile", "mock",
                               "--base-url", self.url, "--model", "mock", "--caps", caps or self.caps,
                               "--spend-log", self.log, *extra],
                              capture_output=True, text=True, timeout=300, env=dict(os.environ, MODEL_API_KEY="mock-key"))

    def test_1_arms_score_and_log_every_call(self):
        for arm in ("noctx", "bm25", "pack"):
            r = self.e2(arm, "e2-" + arm)
            self.assertEqual(r.returncode, 0, r.stderr[-1500:])
            rows = read_jsonl(os.path.join(self.tmp, "e2-" + arm, "rows.jsonl"))
            self.assertEqual(len(rows), 2)
            self.assertFalse(any(x["error"] for x in rows))
            for x in rows:
                self.assertIn("hit5", x)
                self.assertTrue(x["cache_key"].startswith(f"swepro:{x['id']}:"), x["cache_key"])
                self.assertEqual(x["context_tokens"] > 0, arm != "noctx")
        reqs = read_jsonl(self.record)
        self.assertEqual({q["prompt_cache_retention"] for q in reqs}, {"24h"})
        self.assertEqual({q["reasoning_effort"] for q in reqs}, {"medium"})
        self.assertEqual({q["store"] for q in reqs}, {False})
        log = read_jsonl(self.log)
        self.assertEqual(len(log), 6)
        self.assertTrue(all(x["benchmark"] == "swe-bench-pro e2" and x["status"] == "ok" for x in log))
        r = self.e2("pack", "e2-pack")  # a second run skips what is done
        self.assertIn("0 of 3 tasks to do (1 left out", r.stdout)
        meta = json.load(open(os.path.join(self.tmp, "e2-noctx", "run.json")))
        self.assertEqual(meta["left_out_e1_failed"], ["instance_acme__cart-3"], "noctx leaves it out too")

    def test_2_the_cap_refuses_to_start(self):
        caps = os.path.join(self.tmp, "tiny-caps.json")
        with open(caps, "w") as f:
            json.dump({"usd": {"mock": 0.000001}}, f)
        r = self.e2("pack", "e2-refused", caps=caps)
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("REFUSED", r.stderr + r.stdout)

    def test_3_a_changed_context_is_refused(self):
        iid = "instance_acme__cart-1"
        p = os.path.join(self.work, "contexts", iid, "bm25-64K.txt")
        keep = open(p).read()
        try:
            with open(p, "a") as f:
                f.write("changed\n")
            r = self.e2("bm25", "e2-changed")
            self.assertNotEqual(r.returncode, 0)
            self.assertIn("does not match", r.stderr + r.stdout)
        finally:
            with open(p, "w") as f:
                f.write(keep)


if __name__ == "__main__":
    unittest.main()
