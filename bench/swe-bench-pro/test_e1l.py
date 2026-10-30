#!/usr/bin/env python3
"""Tests of e1l.py: the gold-line rules on hand-made diffs, the pack's text and outline lines, and one task end to
end on a small synthetic repository (no SWE-bench Pro data).

usage: test_e1l.py --graphicle BIN --tokenizer TOKENIZER.json
"""
import argparse
import hashlib
import json
import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import e1l  # noqa: E402


def lines_of(diff):
    return diff.split("\n")


def test_gold_lines():
    # '-' lines are gold; context and '+' lines are not.
    g, minus, h = e1l.gold_lines(lines_of("@@ -10,5 +10,5 @@\n a\n b\n-c\n+C\n d\n e"))
    assert (g, minus, h) == ([12], {12: "c"}, 1), (g, minus, h)
    # A hunk without '-' lines: the pre-image line just before each run of '+' lines.
    g, _m, _h = e1l.gold_lines(lines_of("@@ -10,6 +10,9 @@\n a\n b\n c\n+x\n+y\n d\n+z\n e\n f"))
    assert g == [12, 13], g
    # A run that opens the file: line 1. A -U0 insertion after line 7 (-7,0): line 7.
    assert e1l.gold_lines(lines_of("@@ -1,2 +1,3 @@\n+x\n a\n b"))[0] == [1]
    assert e1l.gold_lines(lines_of("@@ -7,0 +8,2 @@\n+x\n+y"))[0] == [7]
    assert e1l.gold_lines(lines_of("@@ -0,0 +1 @@\n+x"))[0] == [1]
    # Content that looks like a header, counted by the hunk's lengths; "\ No newline" is skipped.
    g, minus, _h = e1l.gold_lines(lines_of("@@ -1,3 +1,3 @@\n--- old comment\n+++ new comment\n a\n-b\n\\ No newline at end of file\n+B\n\\ No newline at end of file"))
    assert g == [1, 3] and minus == {1: "-- old comment", 3: "b"}, (g, minus)
    # Two hunks; the second starts after the first's lines.
    g, _m, h = e1l.gold_lines(lines_of("@@ -1,2 +1,2 @@\n-a\n+A\n b\n@@ -20,2 +20,3 @@\n x\n+y\n z"))
    assert (g, h) == ([1, 20], 2), (g, h)


def test_sections():
    patch = ("diff --git a/src/a.py b/src/a.py\nindex 1..2 100644\n--- a/src/a.py\n+++ b/src/a.py\n"
             "@@ -2,2 +2,2 @@\n x\n-y\n+Y\n"
             "diff --git a/old.py b/new.py\nsimilarity index 100%\nrename from old.py\nrename to new.py\n"
             "diff --git a/gone.py b/gone.py\ndeleted file mode 100644\n--- a/gone.py\n+++ /dev/null\n"
             "@@ -1,2 +0,0 @@\n-p\n-q\n"
             "diff --git a/added.py b/added.py\nnew file mode 100644\n--- /dev/null\n+++ b/added.py\n"
             "@@ -0,0 +1 @@\n+n\n")
    secs = {old: (status, e1l.gold_lines(lines)[0]) for old, status, lines in e1l.file_sections(patch)}
    assert secs["src/a.py"] == ("modified", [3]), secs
    assert secs["old.py"] == ("renamed", []), secs
    assert secs["gone.py"] == ("deleted", [1, 2]), secs
    assert secs["added.py"][0] == "added", secs


def test_pack_view():
    pj = {"sections": [
        {"chunks": [{"path": "a.py", "start_line": 1, "end_line": 9, "whole": True, "why": "focus: rank 1"},
                    {"path": "b.py", "start_line": 3, "end_line": 40, "whole": False,
                     "why": "x (outline: the first line of each symbol)",
                     "text": "### b.py lines 3-40 (outline)\nwhy: x\n```python\n3\tdef f():\n40\tdef g():\n```\n"},
                    {"path": "c.py", "start_line": 10, "end_line": 20, "whole": False, "why": "focus: rank 2"},
                    {"path": "c.py", "start_line": 21, "end_line": 25, "whole": False, "why": "call paths"},
                    {"path": None, "start_line": None, "end_line": None, "whole": False, "why": "map"}]}]}
    v = e1l.pack_view(pj, ["a.py", "b.py", "c.py", "d.py"])
    assert v["a.py"]["whole"] and v["a.py"]["ranges"] == [[1, 9]], v
    assert v["b.py"]["ranges"] == [] and v["b.py"]["outline"] == {3, 40}, v
    assert v["c.py"]["ranges"] == [[10, 25]] and not v["c.py"]["whole"], v
    assert v["d.py"] == {"ranges": [], "outline": set(), "whole": False}, v
    assert e1l.covered(12, v["c.py"]["ranges"]) and not e1l.covered(26, v["c.py"]["ranges"])


FILES = {
    "src/billing/invoice_total.py": "".join(f"# line {i}\n" for i in range(1, 6)) +
    "def invoice_total(lines):\n    return sum(round(l.amount * (1 + l.tax_rate), 2) for l in lines)\n",
    "src/billing/currency.py": "CURRENCIES = {'EUR': 2, 'JPY': 0}\n\ndef minor_units(code):\n    return CURRENCIES[code]\n",
    "tests/test_invoice_total.py": "from billing.invoice_total import invoice_total\n\ndef test_empty():\n"
                                   "    assert invoice_total([]) == 0\n",
}
QUESTION = "Invoice total is wrong when the tax rate has three decimals: invoice_total rounds each line."
PATCH = ("diff --git a/src/billing/invoice_total.py b/src/billing/invoice_total.py\n--- a/src/billing/invoice_total.py\n"
         "+++ b/src/billing/invoice_total.py\n@@ -5,3 +5,3 @@\n # line 5\n def invoice_total(lines):\n"
         "-    return sum(round(l.amount * (1 + l.tax_rate), 2) for l in lines)\n"
         "+    return round(sum(l.amount * (1 + l.tax_rate) for l in lines), 2)\n")


def sh(*cmd, cwd=None):
    subprocess.run(cmd, cwd=cwd, check=True, capture_output=True)


def test_task(graphicle, tokenizer):
    import pyarrow as pa
    import pyarrow.parquet as pq
    with tempfile.TemporaryDirectory() as tmp:
        repo = os.path.join(tmp, "upstream")
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
        pq.write_table(pa.table({"instance_id": ["t1"], "patch": [PATCH]}), os.path.join(tmp, "d.parquet"))
        e1l_py = os.path.join(HERE, "e1l.py")
        r = subprocess.run([sys.executable, e1l_py, "gold", "--work", work, "--parquet", os.path.join(tmp, "d.parquet")],
                           capture_output=True, text=True)
        assert r.returncode == 0 and "1 gold lines" in r.stdout, r.stdout + r.stderr
        g = json.loads(open(os.path.join(work, "e1l", "gold.jsonl")).readline())
        assert g["files"]["src/billing/invoice_total.py"]["lines"] == [7], g
        r = subprocess.run([sys.executable, e1l_py, "task", "--work", work, "--graphicle", graphicle, "--tokenizer",
                            tokenizer, "--id", "t1", "--run", "e1l-t"], capture_output=True, text=True)
        assert r.returncode == 0, r.stdout[-2000:] + r.stderr[-2000:]
        rows = [json.loads(l) for l in open(os.path.join(work, "out", "e1l-t", "rows", "t1.jsonl"))]
        info = json.load(open(os.path.join(work, "out", "e1l-t", "tasks", "t1.json")))
        assert info["check"] == {"minus_checked": 1, "minus_mismatch": [], "point_out_of_range": []}, info["check"]
        assert sorted({(x["arm"], x["budget"]) for x in rows}) == sorted(
            (arm, b) for arm in ("pack", "bm25", "trunc") for b in ("16K", "32K", "64K", "128K", "256K")), rows
        for x in rows:
            assert x["gold_lines"] == 1 and x["files_with_lines"] == 1, x
            if x["arm"] in ("bm25", "trunc"):
                assert x["all_lines"] == 1 and x["files"][0]["whole"], x
            else:
                f = x["files"][0]
                assert f["in_context"] and f["text"] and x["all_lines"] == 1 and x["line_share"] == 1.0, x
        assert not os.path.exists(os.path.join(work, "runs", "e1l-t", "src", "t1"))
        # --reuse: the pack measured again, BM25 and truncation rows taken as they are.
        r = subprocess.run([sys.executable, e1l_py, "run", "--work", work, "--graphicle", graphicle, "--tokenizer",
                            tokenizer, "--run", "e1l-r", "--reuse", "e1l-t", "--jobs", "1"], capture_output=True,
                           text=True)
        assert r.returncode == 0, r.stdout[-2000:] + r.stderr[-2000:]
        again = [json.loads(l) for l in open(os.path.join(work, "out", "e1l-r", "rows", "t1.jsonl"))]
        key = lambda x: (x["arm"], x["budget"])  # noqa: E731
        assert sorted(map(key, again)) == sorted(map(key, rows)), again
        for x, y in zip(sorted(rows, key=key), sorted(again, key=key)):
            if x["arm"] == "pack":
                assert (x["lines_inside"], x["file_inside"]) == (y["lines_inside"], y["file_inside"]), (x, y)
            else:
                assert x == y, (x, y)
        rinfo = json.load(open(os.path.join(work, "out", "e1l-r", "tasks", "t1.json")))
        assert rinfo["reuse"] == "e1l-t" and rinfo["budgets"] == info["budgets"], rinfo
        assert rinfo["check"] == info["check"] and rinfo["sizes"] == info["sizes"], rinfo
        r = subprocess.run([sys.executable, os.path.join(HERE, "e1l_compare.py"), "--work", work, "--base", "e1l-t",
                            "--new", "e1l-r", "--json", os.path.join(tmp, "cmp.json")], capture_output=True, text=True)
        assert r.returncode == 0 and "E1L (a): every gold line inside" in r.stdout, r.stdout + r.stderr
        cmp_ = json.load(open(os.path.join(tmp, "cmp.json")))
        assert cmp_["tasks_both"] == 1 and cmp_["budgets"]["64K"]["a"]["change"] == 0.0, cmp_
        r = subprocess.run([sys.executable, os.path.join(HERE, "e1l_report.py"), "--work", work, "--run", "e1l-t",
                            "--e1-run", "none"], capture_output=True, text=True)
        assert r.returncode == 0 and "(a) Tasks with every gold line inside" in r.stdout, r.stdout + r.stderr


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--graphicle", required=True)
    ap.add_argument("--tokenizer", required=True)
    a = ap.parse_args()
    test_gold_lines()
    test_sections()
    test_pack_view()
    test_task(a.graphicle, a.tokenizer)
    print("test_e1l: ok")


if __name__ == "__main__":
    main()
