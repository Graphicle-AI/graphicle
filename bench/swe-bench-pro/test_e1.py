#!/usr/bin/env python3
"""End-to-end test of e1.py and bm25.py on a small synthetic repository (no SWE-bench Pro data).

usage: test_e1.py --graphicle BIN --tokenizer TOKENIZER.json
Builds a git repository with a few files, a bare clone of it as the mirror, one task whose gold file is
known, runs `e1.py task --contexts` and checks the rows: three arms at the five budgets, the gold file
inside BM25's context, budgets kept, and the 64K contexts saved. Then a named run in its own folders, and
pack-only runs (--reuse) that take the first run's BM25 and truncation rows as they are.
"""
import argparse
import hashlib
import json
import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "whole-repo"))
sys.path.insert(0, HERE)
from bm25 import Index, analyze, rank  # noqa: E402
import e1 as e1mod  # noqa: E402

FILES = {
    "src/billing/invoice_total.py": "def invoice_total(lines):\n    \"\"\"Sum the invoice lines, rounding each tax.\"\"\"\n"
                                     "    return sum(round(l.amount * (1 + l.tax_rate), 2) for l in lines)\n",
    "src/billing/currency.py": "CURRENCIES = {'EUR': 2, 'JPY': 0}\n\ndef minor_units(code):\n    return CURRENCIES[code]\n",
    "src/web/routes.py": "from billing.invoice_total import invoice_total\n\ndef get_invoice(req):\n"
                         "    return {'total': invoice_total(req.lines)}\n",
    "tests/test_invoice_total.py": "from billing.invoice_total import invoice_total\n\ndef test_empty():\n"
                                   "    assert invoice_total([]) == 0\n",
    "docs/billing.md": "# Billing\n\nInvoices are summed line by line.\n",
}
QUESTION = "Invoice total is wrong when the tax rate has three decimals: invoice_total rounds each line."


def sh(*cmd, cwd=None):
    subprocess.run(cmd, cwd=cwd, check=True, capture_output=True)


def test_analyzer():
    c = analyze("getUserName HTTPServer snake_case the a x")
    for t in ("getusername", "get", "user", "name", "httpserver", "http", "server", "snake_case", "snake", "case"):
        assert c[t] >= 1, t
    assert "the" not in c and "a" not in c and "x" not in c
    docs = [("a.py", "invoice total invoice"), ("b.py", "unrelated text"), ("c.py", "total"),
            ("d/getUserTotal.py", "def getUserTotal(invoice_lines): return sum(invoice_lines)")]
    r = rank("invoice total rounding", docs)
    ranked = [p for p, _s in r]
    assert ranked[0] == "a.py" and "b.py" not in ranked and set(ranked) == {"a.py", "c.py", "d/getUserTotal.py"}, r
    # The inverted index scores like rank(), up to the order floating-point terms are added.
    idx = Index(docs)
    for query in ("invoice total rounding", "getUserTotal of the invoice lines", "nothing matches here"):
        a, b = rank(query, docs), idx.rank(query)
        assert [p for p, _s in a] == [p for p, _s in b], (query, a, b)
        assert all(abs(x[1] - y[1]) < 1e-9 for x, y in zip(a, b)), (query, a, b)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--graphicle", required=True)
    ap.add_argument("--tokenizer", required=True)
    a = ap.parse_args()
    test_analyzer()
    with tempfile.TemporaryDirectory() as tmp:
        repo = os.path.join(tmp, "upstream")
        os.makedirs(repo)
        for p, t in FILES.items():
            os.makedirs(os.path.dirname(os.path.join(repo, p)), exist_ok=True)
            with open(os.path.join(repo, p), "w") as f:
                f.write(t)
        sh("git", "init", "-q", cwd=repo)
        sh("git", "add", "-A", cwd=repo)
        sh("git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "base", cwd=repo)
        base = subprocess.run(["git", "rev-parse", "HEAD"], cwd=repo, capture_output=True, text=True).stdout.strip()
        work = os.path.join(tmp, "work")
        os.makedirs(os.path.join(work, "mirror"))
        os.makedirs(os.path.join(work, "texts"))
        sh("git", "clone", "-q", "--bare", repo, os.path.join(work, "mirror", "acme__shop.git"))
        task = {"instance_id": "t1", "repo": "acme/shop", "base_commit": base, "language": "python",
                "gold": ["src/billing/invoice_total.py"], "added": [],
                "text_sha256": hashlib.sha256(QUESTION.encode()).hexdigest(), "names_gold_path": False, "hard": False}
        with open(os.path.join(work, "tasks.jsonl"), "w") as f:
            f.write(json.dumps(task) + "\n")
        with open(os.path.join(work, "texts", "t1.txt"), "w") as f:
            f.write(QUESTION)
        r = subprocess.run([sys.executable, os.path.join(HERE, "e1.py"), "task", "--work", work, "--graphicle",
                            a.graphicle, "--tokenizer", a.tokenizer, "--id", "t1", "--contexts"],
                           capture_output=True, text=True)
        assert r.returncode == 0, r.stdout[-2000:] + r.stderr[-2000:]
        rows = [json.loads(l) for l in open(os.path.join(work, "out", "e1", "rows", "t1.jsonl"))]
        info = json.load(open(os.path.join(work, "out", "e1", "tasks", "t1.json")))
        assert sorted({(x["arm"], x["budget"]) for x in rows}) == sorted(
            (arm, b) for arm in ("pack", "bm25", "trunc") for b in ("16K", "32K", "64K", "128K", "256K")), rows
        for x in rows:
            assert x["context_tokens"] is not None and x["context_tokens"] <= x["budget_tokens"], x
            assert x["fits_whole"] is True, x
            if x["arm"] in ("bm25", "trunc"):
                assert x["all_gold"] == 1, x
            else:
                assert 0 <= x["inside_outline_only"] + x["inside_whole"] <= x["inside"], x
        pj = {"sections": [{"chunks": [{"path": "a.py", "whole": False, "why": "map (outline: the first line of each "
                                        "symbol)"}, {"path": "b.py", "whole": True, "why": "focus"},
                                       {"path": "c.py", "whole": False, "why": "x (outline: the first line)"}]},
                           {"chunks": [{"path": "c.py", "whole": False, "why": "focus: rank 2"}, {"path": None}]}]}
        assert e1mod.outline_only(pj) == {"a.py"}, e1mod.outline_only(pj)
        assert info["index"]["wall_seconds"] >= 0 and info["tokens_eligible"] > 0, info
        assert set(info["contexts"]) == {"pack-64K.xml", "bm25-64K.txt"}, info
        for fname in info["contexts"]:
            assert os.path.getsize(os.path.join(work, "contexts", "t1", fname)) > 0
        assert not os.path.exists(os.path.join(work, "src", "t1")), "the checkout is removed after the task"
        # A second, named run (another build) keeps its own folders and, with the same binary, the same rows.
        r = subprocess.run([sys.executable, os.path.join(HERE, "e1.py"), "task", "--work", work, "--graphicle",
                            a.graphicle, "--tokenizer", a.tokenizer, "--id", "t1", "--contexts", "--run", "e1-two"],
                           capture_output=True, text=True)
        assert r.returncode == 0, r.stdout[-2000:] + r.stderr[-2000:]
        rows2 = [json.loads(l) for l in open(os.path.join(work, "out", "e1-two", "rows", "t1.jsonl"))]
        key = lambda x: (x["arm"], x["budget"], x["inside"], x["context_tokens"], x["context_files"])  # noqa: E731
        assert sorted(map(key, rows)) == sorted(map(key, rows2)), "the same binary gives the same rows"
        assert os.path.exists(os.path.join(work, "contexts-e1-two", "t1", "pack-64K.xml"))
        assert not os.path.exists(os.path.join(work, "runs", "e1-two", "src", "t1"))
        cmp = subprocess.run([sys.executable, os.path.join(HERE, "e1_compare.py"), "--work", work, "--a", "e1",
                              "--b", "e1-two"], capture_output=True, text=True)
        assert cmp.returncode == 0 and "pack 5 of 5; bm25 5 of 5; trunc 5 of 5" in cmp.stdout, cmp.stdout + cmp.stderr
        # --reuse: the pack measured again (index built again), BM25 and truncation taken from run e1 as they are.
        e1 = [sys.executable, os.path.join(HERE, "e1.py")]
        opts = ["--work", work, "--graphicle", a.graphicle, "--tokenizer", a.tokenizer]
        r = subprocess.run(e1 + ["task"] + opts + ["--id", "t1", "--run", "e1-pack", "--reuse", "e1"],
                           capture_output=True, text=True)
        assert r.returncode == 0, r.stdout[-2000:] + r.stderr[-2000:]
        rows3 = [json.loads(l) for l in open(os.path.join(work, "out", "e1-pack", "rows", "t1.jsonl"))]
        info3 = json.load(open(os.path.join(work, "out", "e1-pack", "tasks", "t1.json")))
        assert [x for x in rows3 if x["arm"] != "pack"] == [x for x in rows if x["arm"] != "pack"], rows3
        assert [(x["arm"], x["budget"]) for x in rows3] == [(x["arm"], x["budget"]) for x in rows], rows3
        assert sorted(map(key, rows3)) == sorted(map(key, rows)), "the same binary gives the same pack rows"
        assert info3["reused"] == {"run": "e1", "arms": ["bm25", "trunc"], "graphicle": info["graphicle"]}, info3
        assert "manifest_seconds" not in info3 and set(info3["arm_seconds"]) == {"pack"}, info3
        assert info3["tokens_eligible"] == info["tokens_eligible"] and info3["index"]["data_bytes"] > 0, info3
        assert not os.path.exists(os.path.join(work, "runs", "e1-pack", "src", "t1"))
        r = subprocess.run(e1 + ["task"] + opts + ["--id", "t1", "--run", "e1-ctx", "--reuse", "e1", "--contexts"],
                           capture_output=True, text=True)
        assert r.returncode != 0 and "--contexts" in r.stderr, r.stdout + r.stderr
        r = subprocess.run(e1 + ["task"] + opts + ["--id", "t1", "--run", "e1-none", "--reuse", "e1-missing"],
                           capture_output=True, text=True)
        assert r.returncode != 0 and "has no rows" in r.stderr, r.stdout + r.stderr
        # The run command passes --reuse on and runs only the tasks the earlier run measured.
        r = subprocess.run(e1 + ["run"] + opts + ["--run", "e1-pack2", "--reuse", "e1", "--jobs", "1", "--threads", "2"],
                           capture_output=True, text=True)
        assert r.returncode == 0 and "which measured 1 of these 1 tasks" in r.stdout, r.stdout + r.stderr
        rows4 = [json.loads(l) for l in open(os.path.join(work, "out", "e1-pack2", "rows", "t1.jsonl"))]
        assert sorted(map(key, rows4)) == sorted(map(key, rows)), rows4
        cmp = subprocess.run([sys.executable, os.path.join(HERE, "e1_compare.py"), "--work", work, "--a", "e1",
                              "--b", "e1-pack"], capture_output=True, text=True)
        assert cmp.returncode == 0 and "pack 5 of 5; bm25 5 of 5; trunc 5 of 5" in cmp.stdout, cmp.stdout + cmp.stderr
        print("test_e1: ok;", {x["arm"]: x["share"] for x in rows if x["budget"] == "16K"},
              "index", info["index"]["wall_seconds"], "s")


if __name__ == "__main__":
    main()
