#!/usr/bin/env python3
"""Tests of the runner, the global spend log, the caps and the estimate, end to end on the mock.

usage: python test_runner.py [-v]
  MOCK_HOST=ADDR       the address the mock listens on (default: the first of `hostname -I`, else
                       127.0.0.1; some machines refuse TCP to 127.0.0.1)
  BENCH_WORK=DIR       optional: a benchmark work folder with kubernetes checked out and indexed, to
  BENCH_GRAPHICLE=BIN  also run the pack arms (and the cache layouts when BIN has them) on 2 questions
  BENCH_TOKENIZER=FILE

Builds a tiny work folder (two git repositories named like benchmark repositories, their manifests,
question and text files), serves mock_server.py in a thread and runs run.py as a subprocess. No model,
no key and no spend: the mock is priced like Muse Spark 1.3 only to exercise the caps.
"""
import fcntl
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import estimate  # noqa: E402
import mock_server  # noqa: E402
from run import BUDGETS, Spend, CapReached, logged_total  # noqa: E402
from tokens import Counter, build_manifest  # noqa: E402



def read_json(path):
    with open(path) as f:
        return json.load(f)


def read_jsonl(path):
    if not os.path.exists(path):
        return []
    with open(path) as f:
        return [json.loads(line) for line in f if line.strip()]


PRICES = read_json(os.path.join(HERE, "prices.json"))
MOCK_PRICE = PRICES["models"]["mock"]

# A stand-in for `graphicle pack` that keeps the cache layout's JSON contract (graphicle.pack.v1 plus
# layout, prefix_share, prefix_tokens, prefix_sha256 and each section's part), for testing the
# runner's and recall.py's layout paths without a Graphicle build. Never used for results.
FAKE_PACK = r'''
import hashlib, json, os, subprocess, sys
args = sys.argv[1:]
if args[:1] == ["--version"]:
    print("graphicle (test stand-in)")
    sys.exit(0)
if args[:2] == ["pack", "--help"]:
    print("usage: graphicle pack [QUESTION] --layout standard|cache --prefix-share F")
    sys.exit(0)
q, opt = args[1], {args[i]: args[i + 1] for i in range(2, len(args) - 1, 2)}
src, budget, fmt = opt["--project"], int(opt["--budget"]), opt["--format"]
layout, share = opt.get("--layout", "standard"), float(opt.get("--prefix-share", "0.9"))
files = subprocess.run(["git", "ls-files"], cwd=src, capture_output=True, text=True).stdout.split()


def chunk(path, text):
    return {"path": path, "start_line": 1, "end_line": text.count("\n"), "whole": True,
            "tokens": len(text.encode()) // 4, "why": "test", "text": text}


def body(p):
    return '<file path="%s">\n' % p + open(os.path.join(src, p)).read() + "</file>\n"


header = chunk("", "# Code pack\n\nRepository map:\n" + "".join("- %s\n" % p for p in files) * 8)
words = [w.lower() for w in q.split() if len(w) > 3]
focus = [p for p in files if any(w in p.lower() for w in words)] or files[:1]
if layout == "cache":
    prefix, used = [header], header["tokens"]
    for p in files:  # question-independent: files by path while the prefix share lasts
        c = chunk(p, body(p))
        if used + c["tokens"] > int(budget * share):
            break
        prefix.append(c)
        used += c["tokens"]
    held = {c["path"] for c in prefix}
    suffix = [chunk(p, body(p)) for p in focus if p not in held] + [chunk("", "Focus: %s\n" % ", ".join(focus))]
    sections = [{"name": "map", "part": "prefix", "chunks": prefix}, {"name": "focus", "part": "suffix", "chunks": suffix}]
else:
    rest = [p for p in files if p not in focus]
    sections = [{"name": "header", "part": "prefix", "chunks": [header]},
                {"name": "focus", "part": "suffix", "chunks": [chunk(p, body(p)) for p in focus + rest]}]
total = 0
for s in sections:
    keep = []
    for c in s["chunks"]:
        if total + c["tokens"] <= budget:
            keep.append(c)
            total += c["tokens"]
    s["chunks"], s["tokens"] = keep, sum(c["tokens"] for c in keep)
pre = [s for s in sections if s["part"] == "prefix"]
prefix_text = "".join(c["text"] for s in pre for c in s["chunks"])
paths = [c["path"] for s in sections for c in s["chunks"] if c["path"]]
pj = {"format": "graphicle.pack.v1", "project": os.path.basename(src), "commit": "test", "question": q,
      "budget": budget, "tokens": total, "tokenizer": "estimate", "order": "relevant-last", "layout": layout,
      "prefix_tokens": sum(s["tokens"] for s in pre), "prefix_sha256": hashlib.sha256(prefix_text.encode()).hexdigest(),
      "sections": sections, "included": [{"path": p, "tokens": 0, "rank": i + 1} for i, p in enumerate(paths)],
      "omitted": [], "seconds": 0.0}
if layout == "cache":
    pj["prefix_share"] = share
if fmt == "json":
    print(json.dumps(pj))
else:
    sys.stdout.write("".join('<section name="%s">\n' % s["name"] + "".join(c["text"] for c in s["chunks"])
                             + "</section>\n" for s in sections))
'''


def mock_host():
    if os.environ.get("MOCK_HOST"):
        return os.environ["MOCK_HOST"]
    try:
        out = subprocess.run(["hostname", "-I"], capture_output=True, text=True, timeout=10).stdout.split()
        if out:
            return out[0]
    except (OSError, subprocess.SubprocessError):
        pass
    return "127.0.0.1"


def git(cwd, *args):
    subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True,
                   env=dict(os.environ, GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@t", GIT_COMMITTER_NAME="t",
                            GIT_COMMITTER_EMAIL="t@t"))


def make_repo(src, files):
    os.makedirs(src)
    for path, text in files.items():
        os.makedirs(os.path.join(src, os.path.dirname(path)), exist_ok=True)
        with open(os.path.join(src, path), "w") as f:
            f.write(text)
    git(src, "init", "-q")
    git(src, "add", "-A")
    git(src, "commit", "-q", "-m", "fixture")
    return subprocess.run(["git", "rev-parse", "HEAD"], cwd=src, capture_output=True, text=True).stdout.strip()


def body(word, n):
    return "".join(f"func {word}Handler{i}(ctx Context) error {{ return check{word}(ctx, {i}) }}\n" for i in range(n))


class RunnerTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="wrb-test-")
        cls.work = os.path.join(cls.tmp, "work")
        cls.qdir = os.path.join(cls.tmp, "questions")
        cls.tdir = os.path.join(cls.tmp, "texts")
        os.makedirs(cls.qdir)
        os.makedirs(cls.tdir)
        layout = {
            "kubernetes": {f"pkg/{w}/{w}.go": body(w, 40) for w in
                           ("scheduler", "kubelet", "proxy", "volume", "eviction", "storage", "features", "apiserver")},
            "pytorch": {f"torch/{w}.py": body(w, 40).replace("func ", "def ") for w in
                        ("autograd", "cache", "dynamo", "inductor", "sparse")},
        }
        issues = {
            "kubernetes": [("scheduler", "The scheduler handler panics when ctx is nil"),
                           ("kubelet", "kubelet handler returns the wrong error"),
                           ("eviction", "eviction handler skips pods"),
                           ("features", "feature gate features handler is ignored")],
            "pytorch": [("autograd", "autograd handler leaks memory"), ("cache", "cache handler is stale"),
                        ("sparse", "sparse handler crashes")],
        }
        counter = Counter(None)
        for name, files in layout.items():
            sha = make_repo(os.path.join(cls.work, "src", name), files)
            build_manifest(cls.work, name, counter)
            slug = {"kubernetes": "kubernetes/kubernetes", "pytorch": "pytorch/pytorch"}[name]
            ext = ".go" if name == "kubernetes" else ".py"
            with open(os.path.join(cls.qdir, f"questions-{name}.jsonl"), "w") as qf, \
                    open(os.path.join(cls.tdir, f"texts-{name}.jsonl"), "w") as tf:
                for i, (word, text) in enumerate(issues[name], 1):
                    gold = f"pkg/{word}/{word}{ext}" if name == "kubernetes" else f"torch/{word}{ext}"
                    qf.write(json.dumps({"id": f"{name}-{i}", "repo": slug, "S": sha, "gold": [gold],
                                         "names_gold_path": False}) + "\n")
                    tf.write(json.dumps({"id": f"{name}-{i}", "text": text}) + "\n")
        cls.record = os.path.join(cls.tmp, "mock-requests.jsonl")
        cls.host = mock_host()
        cls.srv = mock_server.make_server(cls.host, 0, record=cls.record)
        threading.Thread(target=cls.srv.serve_forever, daemon=True).start()
        cls.url = f"http://{cls.host}:{cls.srv.server_address[1]}/v1"
        cls.fake = os.path.join(cls.tmp, "fake-graphicle")
        with open(cls.fake, "w") as f:
            f.write(f"#!{sys.executable}\n" + FAKE_PACK)
        os.chmod(cls.fake, 0o755)
        cls.log = os.path.join(cls.tmp, "spend", "spend-log.jsonl")
        cls.caps = os.path.join(cls.tmp, "caps.json")
        with open(cls.caps, "w") as f:
            json.dump({"usd": {"mock": 5.0}}, f)
        cls.runs = []

    @classmethod
    def tearDownClass(cls):
        cls.srv.shutdown()
        cls.srv.server_close()
        if not os.environ.get("KEEP_TEST_DIR"):
            shutil.rmtree(cls.tmp, ignore_errors=True)

    def run_py(self, out, *args, caps=None, ok=True):
        cmd = [sys.executable, os.path.join(HERE, "run.py"), "--work", self.work, "--questions", self.qdir,
               "--texts", self.tdir, "--out", os.path.join(self.tmp, out), "--base-url", self.url,
               "--model", "mock", "--caps", caps or self.caps, "--spend-log", self.log, *args]
        if "--profile" not in args:
            cmd += ["--profile", "mock"]
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=600, env=dict(os.environ, MODEL_API_KEY="mock-key",
                                                                                  BASETEN_API_KEY="mock-key"))
        if ok:
            self.assertEqual(r.returncode, 0, r.stderr[-2000:])
            self.runs.append(os.path.join(self.tmp, out))
        return r

    def rows(self, out):
        return read_jsonl(os.path.join(self.tmp, out, "rows.jsonl"))

    def requests(self):
        return read_jsonl(self.record)

    def log_lines(self, out=None):
        lines = read_jsonl(self.log)
        return [x for x in lines if out is None or x["run"] == os.path.join(self.tmp, out)]

    def test_1_truncation_is_cached_after_the_first_question(self):
        before = len(self.requests())
        self.run_py("trunc", "--arm", "trunc", "--budget", "64K", "--repos", "kubernetes", "--jobs", "2")
        rows = {r["id"]: r for r in self.rows("trunc")}
        self.assertEqual(len(rows), 4)
        self.assertFalse(any(r["error"] for r in rows.values()))
        self.assertEqual(rows["kubernetes-1"]["cached_per_call"], [0], "the first question runs alone and pays in full")
        for qid in ("kubernetes-2", "kubernetes-3", "kubernetes-4"):
            self.assertGreaterEqual(rows[qid]["tokens_cached"], 1024, qid)
        reqs = self.requests()[before:]
        keys = {r["prompt_cache_key"] for r in reqs}
        self.assertEqual(len(keys), 1, "one cache key for the group")
        key = keys.pop()
        self.assertTrue(key.startswith("wrb:kubernetes@") and ":trunc@64K:" in key, key)
        self.assertNotIn("handler", key, "no question text in the key")
        self.assertEqual({r["prompt_cache_retention"] for r in reqs}, {"24h"})
        self.assertEqual({r["max_output_tokens"] for r in reqs}, {8192})
        self.assertEqual({r["reasoning_effort"] for r in reqs}, {"medium"})
        self.assertEqual({r["store"] for r in reqs}, {False})

    def test_2_agent_takes_the_repositories_in_turn(self):
        self.run_py("agent", "--arm", "agent", "--repos", "kubernetes,pytorch", "--max-turns", "4", "--jobs", "1")
        rows = self.rows("agent")
        self.assertEqual(len(rows), 7)
        self.assertFalse(any(r["error"] for r in rows))
        first_turns = [x["question"] for x in self.log_lines("agent") if x["turn"] == 1]
        self.assertEqual(first_turns[:4], ["kubernetes-1", "pytorch-1", "kubernetes-2", "pytorch-2"])
        self.assertTrue(all(r["turns"] > 1 for r in rows))
        self.assertTrue(any(c > 0 for r in rows for c in r["cached_per_call"][1:]), "chained turns read the cache")

    def test_3_baseten_profile_sends_session_affinity_not_the_key(self):
        before = len(self.requests())
        self.run_py("baseten-trunc", "--profile", "baseten", "--arm", "trunc", "--budget", "64K",
                    "--repos", "pytorch", "--jobs", "1")
        reqs = self.requests()[before:]
        self.assertEqual(len(reqs), 3)
        self.assertTrue(all(r["path"].endswith("/chat/completions") for r in reqs))
        self.assertEqual({r["prompt_cache_key"] for r in reqs}, {None})
        aff = {r["session_affinity"] for r in reqs}
        self.assertEqual(len(aff), 1)
        self.assertRegex(aff.pop(), r"^[0-9a-f]{32}$")
        self.assertEqual({r["max_tokens"] for r in reqs}, {8192})
        self.assertEqual({r["reasoning_effort"] for r in reqs}, {"medium"})
        rows = {r["id"]: r for r in self.rows("baseten-trunc")}
        self.assertEqual(rows["pytorch-1"]["tokens_cached"], 0)
        self.assertGreaterEqual(rows["pytorch-2"]["tokens_cached"], 1024)

    def test_4_global_cap_refuses_to_start(self):
        tight = os.path.join(self.tmp, "caps-tight.json")
        with open(tight, "w") as f:
            json.dump({"usd": {"mock": logged_total(self.log, "mock") + 0.01}}, f)
        before = len(self.requests())
        r = self.run_py("refused", "--arm", "trunc", "--budget", "128K", "--repos", "kubernetes", caps=tight, ok=False)
        self.assertEqual(r.returncode, 3)
        self.assertIn("REFUSED", r.stderr)
        self.assertEqual(len(self.requests()), before, "no call was sent")

    def test_5_run_cap_stops_the_run(self):
        r = self.run_py("capped", "--arm", "trunc", "--budget", "64K", "--repos", "kubernetes", "--jobs", "1",
                        "--spend-cap", "0.05", "--label", "trunc-capped")
        self.assertIn("STOPPED", r.stdout)
        rows = self.rows("capped")
        self.assertEqual(len(rows), 1)
        spent = read_json(os.path.join(self.tmp, "capped", "spend.json"))["spent_usd"]
        self.assertLessEqual(spent, 0.05)
        meta = read_json(os.path.join(self.tmp, "capped", "run.json"))
        self.assertIn("run spend cap", meta["stopped"])

    def test_5b_auto_cap_is_one_and_a_half_estimates_within_the_global_cap(self):
        self.run_py("auto-cap", "--arm", "trunc", "--budget", "64K", "--repos", "kubernetes", "--spend-cap", "auto",
                    "--label", "trunc-auto")
        meta = read_json(os.path.join(self.tmp, "auto-cap", "run.json"))
        est = meta["estimate_usd"]["cached"]
        self.assertEqual(meta["spend_cap_usd"], float(-(-1.5 * est // 1)))  # ceil, in whole dollars
        tight = os.path.join(self.tmp, "caps-auto.json")
        left = 0.5
        with open(tight, "w") as f:
            json.dump({"usd": {"mock": logged_total(self.log, "mock") + left}}, f)
        self.run_py("auto-cap-tight", "--arm", "trunc", "--budget", "64K", "--repos", "pytorch", "--spend-cap", "auto",
                    "--label", "trunc-auto-tight", caps=tight)
        meta = read_json(os.path.join(self.tmp, "auto-cap-tight", "run.json"))
        self.assertAlmostEqual(meta["spend_cap_usd"], left, places=6)

    def test_6_global_cap_stops_a_call_mid_run(self):
        log = os.path.join(self.tmp, "unit-log.jsonl")
        with open(log, "w") as f:
            f.write(json.dumps({"provider": "mock", "cost_usd": 0.95}) + "\n")
        s = Spend(os.path.join(self.tmp, "unit-spend.json"), 10.0, MOCK_PRICE, log, "mock", 1.0)
        w = s.reserve(10000, 1000)  # $0.0168 worst case fits under the $0.05 left
        s.settle(w, 0.01, {"provider": "mock", "cost_usd": 0.01})
        with self.assertRaises(CapReached) as cm:
            s.reserve(100000, 8192)  # $0.16 worst case does not
        self.assertIn("global mock cap", str(cm.exception))
        self.assertAlmostEqual(logged_total(log, "mock"), 0.96)

    def test_7_a_failing_call_is_tried_again_once_then_kept_as_an_error(self):
        self.run_py("errors", "--arm", "noctx", "--repos", "pytorch", "--reasoning-effort", "none")
        rows = self.rows("errors")
        self.assertEqual(len(rows), 3)
        self.assertTrue(all("HTTP 400" in r["error"] for r in rows))
        lines = self.log_lines("errors")
        self.assertEqual(len(lines), 6, "each question: the call and its one retry")
        self.assertEqual({x["status"] for x in lines}, {"error"})
        self.assertEqual(sum(x["cost_usd"] for x in lines), 0.0)

    def test_8_a_second_run_of_the_provider_is_refused(self):
        with open(self.log + ".mock.lock", "w") as held:
            fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
            r = self.run_py("locked", "--arm", "noctx", "--repos", "pytorch", ok=False)
        self.assertEqual(r.returncode, 3)
        self.assertIn("another run", r.stderr)

    def test_9_the_global_log_adds_up(self):
        lines = self.log_lines()
        self.assertTrue(lines)
        for x in lines:
            want = ((x["input_tokens"] - x["cached_tokens"]) * MOCK_PRICE["input"]
                    + x["cached_tokens"] * MOCK_PRICE["cached_input"] + x["output_tokens"] * MOCK_PRICE["output"]) / 1e6
            self.assertAlmostEqual(x["cost_usd"], want, places=12)
            for k in ("time", "model", "arm", "repo", "input_tokens", "cached_tokens", "output_tokens",
                      "reasoning_tokens", "seconds", "cost_usd", "provider"):
                self.assertIn(k, x)
        for d in self.runs:
            rows = read_jsonl(os.path.join(d, "rows.jsonl"))
            spent = read_json(os.path.join(d, "spend.json"))["spent_usd"]
            logged = sum(x["cost_usd"] for x in lines if x["run"] == d)
            self.assertAlmostEqual(spent, logged, places=9, msg=d)
            if not os.path.basename(d).startswith("capped"):
                self.assertAlmostEqual(sum(r.get("cost_usd") or 0 for r in rows), logged, places=9, msg=d)
        self.assertAlmostEqual(logged_total(self.log, "mock"), sum(x["cost_usd"] for x in lines), places=9)

    def test_c1_cache_layout_arm_reads_its_prefix_from_the_cache(self):
        before = len(self.requests())
        self.run_py("pack-cache", "--arm", "pack-cache", "--budget", "64K", "--repos", "kubernetes", "--jobs", "2",
                    "--graphicle", self.fake)
        rows = {r["id"]: r for r in self.rows("pack-cache")}
        self.assertEqual(len(rows), 4)
        self.assertFalse(any(r["error"] for r in rows.values()))
        self.assertTrue(all(r["prefix_ok"] and r["layout"] == "cache" and r["prefix_share"] == 0.9
                            for r in rows.values()))
        self.assertEqual(len({r["prefix_sha256"] for r in rows.values()}), 1)
        self.assertEqual(rows["kubernetes-1"]["tokens_cached"], 0)
        for qid in ("kubernetes-2", "kubernetes-3", "kubernetes-4"):
            self.assertGreaterEqual(rows[qid]["tokens_cached"], 1024, qid)
        keys = {r["prompt_cache_key"] for r in self.requests()[before:]}
        self.assertEqual(len(keys), 1)
        self.assertIn(":pack-cache@64K:" + rows["kubernetes-1"]["prefix_sha256"][:12], keys.pop())

    def test_c2_recall_layout_grid(self):
        out = os.path.join(self.tmp, "recall-layouts")
        r = subprocess.run([sys.executable, os.path.join(HERE, "recall.py"), "--work", self.work, "--questions",
                            self.qdir, "--texts", self.tdir, "--graphicle", self.fake, "--out", out,
                            "--repos", "kubernetes,pytorch", "--budgets", "64K,128K",
                            "--layouts", "standard,cache0.9,cache0.95,stable", "--jobs", "2"],
                           capture_output=True, text=True, timeout=600)
        self.assertEqual(r.returncode, 0, r.stderr[-2000:])
        summary = read_json(os.path.join(out, "recall-summary.json"))
        self.assertEqual(summary["errors"], 0)
        rows = [s for s in summary["rows"] if s["scope"] == "all" and s["arm"] != "truncation"]
        self.assertEqual({s["arm"] for s in rows}, {"pack", "pack-cache", "pack-cache0.95", "pack-stable"})
        self.assertEqual(len(rows), 4 * 2 * 2)
        for s in rows:
            self.assertEqual(s["prefix_ok"], s["n"], s)
            self.assertGreater(s["question_tokens"], 0)
            if s["arm"] != "pack":
                self.assertEqual(s["prefix_shas"], 1, f"one prefix per repository, budget and layout: {s}")
                self.assertGreater(s["prefix_tokens"], 1024)
        stable = [x for x in read_jsonl(os.path.join(out, "recall-rows.jsonl")) if x["arm"] == "pack-stable"]
        self.assertTrue(stable and all(x["prefix_share"] == 1.0 for x in stable))

    def test_redact_strips_emails_and_mentions_but_not_code(self):
        from redact import redact
        text = ("cc @alice and @bob-x: see a.b@example.com. @torch.compile and @types/node stay, "
                "`@property` stays\n```\n@Override\n@someone\n```\n@carol")
        new, emails, handles = redact(text)
        self.assertEqual((emails, handles), (1, 3))
        self.assertNotIn("alice", new)
        self.assertNotIn("bob-x", new)
        self.assertNotIn("carol", new)
        self.assertNotIn("example.com", new)
        for kept in ("@torch.compile", "@types/node", "`@property`", "@Override", "@someone"):
            self.assertIn(kept, new)
        self.run_py("redacted", "--arm", "noctx", "--repos", "pytorch", "--redact")
        rows = self.rows("redacted")
        self.assertTrue(rows and all(r["redacted"] == {"emails": 0, "handles": 0} for r in rows))
        self.assertTrue(read_json(os.path.join(self.tmp, "redacted", "run.json"))["redact"])

    def test_g1_bm25_arm_fills_its_budget_with_ranked_files(self):
        self.run_py("bm25", "--arm", "bm25", "--budget", "16K", "--repos", "kubernetes")
        rows = {r["id"]: r for r in self.rows("bm25")}
        self.assertEqual(len(rows), 4)
        self.assertFalse(any(r["error"] for r in rows.values()))
        for r in rows.values():
            self.assertLessEqual(r["context_tokens"], BUDGETS["16K"])
            self.assertEqual(r["context_recall"], 1.0, r["id"])  # the issue names its handler's words
        keys = {x["cache_key"] for x in self.log_lines("bm25")}
        self.assertTrue(all(":bm25@16K:" in k for k in keys), keys)

    def test_g2_agent_pack_hybrid_seeds_the_first_message(self):
        self.run_py("agent-pack", "--arm", "agent-pack", "--budget", "32K", "--repos", "kubernetes", "--max-turns", "3",
                    "--graphicle", self.fake)
        rows = self.rows("agent-pack")
        self.assertEqual(len(rows), 4)
        self.assertFalse(any(r["error"] for r in rows), [r["error"] for r in rows])
        for r in rows:
            self.assertIsNotNone(r["seed_recall"])
            self.assertGreaterEqual(r["context_recall"], r["seed_recall"])
            self.assertGreater(r["context_tokens"], 0)
            self.assertLessEqual(r["turns"], 4)
            self.assertEqual(len(r["in_per_call"]), len(r["cached_per_call"]))
        meta = read_json(os.path.join(self.tmp, "agent-pack", "run.json"))
        self.assertEqual((meta["max_turns"], meta["agent_max_input"], meta["tool_search"]), (3, 2000000, False))

    def test_g3_tool_search_loads_the_deferred_tools(self):
        before = len(self.requests())
        self.run_py("tool-search", "--arm", "agent", "--repos", "pytorch", "--max-turns", "4", "--tool-search")
        rows = self.rows("tool-search")
        self.assertEqual(len(rows), 3)
        self.assertFalse(any(r["error"] for r in rows), [r["error"] for r in rows])
        self.assertTrue(all(r["tool_search"] and r["tool_calls"] > 0 for r in rows))
        reqs = [r for r in self.requests()[before:] if r["tools"]]
        self.assertTrue(reqs and all(r["tool_types"] == ["namespace", "tool_search"] for r in reqs))
        raw = read_json(os.path.join(self.tmp, "tool-search", "raw", "pytorch-1.json"))
        types = [o["type"] for resp in raw["responses"] for o in resp["output"]]
        self.assertEqual(types.count("tool_search_call"), 1, "the namespace loads once per question")
        self.assertTrue(all(o.get("namespace") == "repo" for resp in raw["responses"] for o in resp["output"]
                            if o["type"] == "function_call"))

    def test_g4_agent_input_cap_ends_the_question(self):
        self.run_py("agent-capped", "--arm", "agent", "--repos", "pytorch", "--max-turns", "30",
                    "--agent-max-input", "600")
        rows = self.rows("agent-capped")
        self.assertFalse(any(r["error"] for r in rows))
        # The first turn's input plus the next prompt (its context, its output and the tool result) pass 600.
        self.assertTrue(all(r["turns"] == 2 for r in rows),
                        [(r["turns"], r["in_per_call"], r["tokens_out"]) for r in rows])

    def test_estimate_caches_the_shared_prefix(self):
        agent = {"turns": 15, "turn_tokens": 2500, "out_per_turn": 800}
        b = BUDGETS["64K"]
        self.assertEqual(estimate.group_tokens("trunc", "64K", 4, 1000, 4000, agent)[2], 3 * b)
        self.assertAlmostEqual(estimate.group_tokens("pack-cache", "64K", 4, 1000, 4000, agent)[2], 3 * 0.9 * b)
        self.assertAlmostEqual(estimate.group_tokens("pack-cache0.95", "64K", 4, 1000, 4000, agent)[2], 3 * 0.95 * b)
        self.assertEqual(estimate.group_tokens("pack-stable", "64K", 4, 1000, 4000, agent)[2], 3 * b)
        self.assertEqual(estimate.group_tokens("pack", "64K", 4, 1000, 4000, agent)[2], 0)
        self.assertEqual(estimate.group_tokens("noctx", None, 4, 1000, 4000, agent)[2], 0)
        calls, tin, cached, tout = estimate.group_tokens("trunc", "64K", 4, 1000, 4000, agent)
        p = PRICES["models"]["muse-spark-1.3"]
        first = estimate.cost_of(p, b + 1000, 0, 4000)
        later = estimate.cost_of(p, b + 1000, b, 4000)
        self.assertAlmostEqual(estimate.cost_of(p, tin, cached, tout), first + 3 * later)
        self.assertLess(later, first)

    def test_pack_arms_on_a_real_index(self):
        work, binary = os.environ.get("BENCH_WORK"), os.environ.get("BENCH_GRAPHICLE")
        if not (work and binary):
            self.skipTest("BENCH_WORK and BENCH_GRAPHICLE not set")
        tok = os.environ.get("BENCH_TOKENIZER")
        has_layout = "--layout" in subprocess.run([binary, "pack", "--help"], capture_output=True, text=True).stdout
        arms = ["pack"] + (["pack-cache", "pack-stable"] if has_layout else [])
        qdir = os.path.join(os.path.dirname(os.path.abspath(__file__)), "questions")
        for arm in arms:
            out = os.path.join(self.tmp, f"real-{arm}")
            cmd = [sys.executable, os.path.join(HERE, "run.py"), "--arm", arm, "--budget", "64K", "--work", work,
                   "--questions", qdir, "--texts", os.path.join(work, "mine"), "--out", out, "--profile", "mock",
                   "--base-url", self.url, "--model", "mock", "--caps", self.caps, "--spend-log", self.log,
                   "--repos", "kubernetes", "--limit", "2", "--graphicle", binary] + (["--tokenizer", tok] if tok else [])
            r = subprocess.run(cmd, capture_output=True, text=True, timeout=1800,
                               env=dict(os.environ, MODEL_API_KEY="mock-key"))
            self.assertEqual(r.returncode, 0, r.stderr[-2000:])
            self.runs.append(out)
            rows = read_jsonl(os.path.join(out, "rows.jsonl"))
            self.assertEqual(len(rows), 2)
            for row in rows:
                self.assertIsNone(row["error"], row["error"])
                self.assertLessEqual(row["context_tokens"], BUDGETS["64K"])
                self.assertIn(f":{arm}@64K:", row["cache_key"])
                if arm != "pack":
                    self.assertTrue(row["prefix_ok"], "the prefix comes first and matches its sha256")
            if arm != "pack":
                self.assertEqual(len({r["prefix_sha256"] for r in rows}), 1, "one prefix for both questions")
                self.assertGreaterEqual(rows[1]["tokens_cached"], 1024)


if __name__ == "__main__":
    unittest.main()
