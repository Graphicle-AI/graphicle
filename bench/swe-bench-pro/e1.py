#!/usr/bin/env python3
"""E1: gold-file recall on SWE-bench Pro without a model: the pack, file-level BM25 and truncation.

usage:
  e1.py run --work WORK --graphicle BIN --tokenizer TOKENIZER.json [--ids FILE] [--order sample|shuffled]
            [--jobs 6] [--threads 6] [--keep]
      runs every task of WORK/tasks.jsonl (or the ids in FILE: a JSON list, or an object with "ids") that has
      no rows yet, each in its own process; --order shuffled runs them in random.Random(20261027) order
  e1.py task --work WORK --graphicle BIN --tokenizer TOKENIZER.json --id INSTANCE_ID [--contexts] [--keep]
      one task: check out the repository at the base commit (WORK/mirror holds a bare clone of each
      repository), build its Graphicle index (timed), count every file with the tokenizer (tokens.py),
      then measure the three arms at every budget and write WORK/out/e1/rows/<id>.jsonl and
      WORK/out/e1/tasks/<id>.json (sizes, index time and size, seconds per arm). --contexts also saves the
      64K pack (xml) and BM25 contexts under WORK/contexts/<id>/ for the answer runs.
  --reuse RUN (run and task) measures the pack again with another build and takes the BM25 and truncation
      rows of an earlier run as they are: those two arms are this script's own code and read the checkout and
      the token counts, never Graphicle, so a new build cannot change them. The task's budgets, file counts and
      tokens come from RUN's record of it (no tokenizer pass), the index is built again by BIN, and the
      record notes the reuse. Only tasks RUN measured are run; --contexts is refused (it needs BM25's text).

The budgets are 16K, 32K, 64K, 128K and 256K tokens, plus 1M (1,015,808) when truncation's file set holds
more than that. A row: id, repo, arm, budget, gold_n, inside (gold files inside the context), share,
all_gold, any_gold, context_tokens, context_files, fits_whole (truncation's file set fits in the budget),
names_gold_path, seconds. A gold file is inside the pack when the pack holds at least one passage of it (its
`included` list, which names every file whose text the pack holds); pack rows also give inside_whole, the gold
files the pack holds whole, and inside_outline_only, the gold files inside whose only text in the pack is an
outline (the first line of each symbol). Both are counts within inside, for separate columns: they never change
it. BM25 and truncation hold whole files only.
"""
import argparse
import hashlib
import json
import os
import random
import shutil
import subprocess
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor

HERE = os.path.dirname(os.path.abspath(__file__))
WR = os.path.join(HERE, "..", "whole-repo")
sys.path.insert(0, WR)
from bm25 import build_bm25  # noqa: E402
from pack_arm import pack_command  # noqa: E402
from tokens import Counter, build_manifest, build_truncation, truncation_order  # noqa: E402

BUDGETS = [("16K", 16384), ("32K", 32768), ("64K", 65536), ("128K", 131072), ("256K", 262144)]
BIG = ("1M", 1015808)
CONTEXT_BUDGET = "64K"
ORDER_SEED = 20261027
MAX_ATTEMPTS = 2


def measured(cmd, timeout):
    """(exit code, stdout, stderr, wall seconds, CPU seconds, peak resident MiB) of one process."""
    with tempfile.TemporaryFile() as out, tempfile.TemporaryFile() as err:
        t0 = time.time()
        p = subprocess.Popen(cmd, stdout=out, stderr=err)
        try:
            _pid, status, ru = os.wait4(p.pid, 0) if timeout is None else _wait(p, timeout)
        except subprocess.TimeoutExpired:
            p.kill()
            _pid, status, ru = os.wait4(p.pid, 0)
            raise
        out.seek(0)
        err.seek(0)
        return (os.waitstatus_to_exitcode(status), out.read().decode("utf-8", "replace"),
                err.read().decode("utf-8", "replace"), time.time() - t0, ru.ru_utime + ru.ru_stime, ru.ru_maxrss / 1024)


def _wait(p, timeout):
    deadline = time.time() + timeout
    while True:
        pid, status, ru = os.wait4(p.pid, os.WNOHANG)
        if pid:
            return pid, status, ru
        if time.time() > deadline:
            raise subprocess.TimeoutExpired(p.args, timeout)
        time.sleep(0.2)


def du_bytes(path):
    total = 0
    for root, _dirs, files in os.walk(path):
        for f in files:
            try:
                total += os.lstat(os.path.join(root, f)).st_size
            except OSError:
                pass
    return total


def load_tasks(work):
    return {t["instance_id"]: t for t in map(json.loads, open(os.path.join(work, "tasks.jsonl"))) if t}


def git(src, *args, check=True):
    return subprocess.run(["git", "-C", src, *args], capture_output=True, text=True, check=check)


def run_paths(work, run):
    """(scratch root of the checkouts, indexes and manifests; output folder; contexts folder; logs folder). The
    first run, "e1", keeps the original layout; a later run (another build) has its own of each, so two runs can
    go at once."""
    if run == "e1":
        return work, os.path.join(work, "out", "e1"), os.path.join(work, "contexts"), os.path.join(work, "logs", "e1")
    return (os.path.join(work, "runs", run), os.path.join(work, "out", run), os.path.join(work, "contexts-" + run),
            os.path.join(work, "logs", run))


def checkout(work, scratch, t):
    mirror = os.path.join(work, "mirror", t["repo"].replace("/", "__") + ".git")
    src = os.path.join(scratch, "src", t["instance_id"])
    if os.path.exists(src):
        shutil.rmtree(src)
    os.makedirs(os.path.dirname(src), exist_ok=True)
    subprocess.run(["git", "clone", "-q", "--shared", "--no-checkout", mirror, src], check=True)
    git(src, "-c", "advice.detachedHead=false", "checkout", "-q", "--detach", t["base_commit"])
    head = git(src, "rev-parse", "HEAD").stdout.strip()
    if head != t["base_commit"]:
        raise RuntimeError(f"checkout gave {head}, not {t['base_commit']}")
    return src


def row(t, arm, label, budget, included, tokens, secs, gold, fits):
    inc = set(included)
    inside = sum(1 for g in gold if g in inc)
    return {"id": t["instance_id"], "repo": t["repo"], "arm": arm, "budget": label, "budget_tokens": budget,
            "gold_n": len(gold), "inside": inside, "share": inside / len(gold), "all_gold": int(inside == len(gold)),
            "any_gold": int(inside > 0), "context_tokens": tokens, "context_files": len(included),
            "fits_whole": fits, "names_gold_path": t["names_gold_path"], "seconds": round(secs, 3)}


REUSED_ARMS = ("bm25", "trunc")


def outline_only(pj):
    """The files a pack holds only as outlines (the first line of each symbol: a chunk whose `why` says outline)."""
    kinds = {}
    for sec in pj.get("sections") or []:
        for c in sec.get("chunks") or []:
            if c.get("path"):
                outline = not c.get("whole") and "(outline" in (c.get("why") or "")
                kinds.setdefault(c["path"], set()).add(outline)
    return {p for p, k in kinds.items() if k == {True}}


def reused_task(work, run, iid):
    """The record and rows of task iid in an earlier run (--reuse), or None when that run did not measure it."""
    _s, outdir, _c, _l = run_paths(work, run)
    rows_p, task_p = os.path.join(outdir, "rows", iid + ".jsonl"), os.path.join(outdir, "tasks", iid + ".json")
    if not (os.path.exists(rows_p) and os.path.exists(task_p)):
        return None
    rows = {(r["arm"], r["budget"]): r for r in map(json.loads, open(rows_p))}
    return {"info": json.load(open(task_p)), "rows": rows}


def cmd_task(a):
    work = os.path.abspath(a.work)
    t = load_tasks(work)[a.id]
    iid = t["instance_id"]
    scratch, outdir, ctx_root, _logs = run_paths(work, a.run)
    out_rows = os.path.join(outdir, "rows", iid + ".jsonl")
    out_task = os.path.join(outdir, "tasks", iid + ".json")
    if os.path.exists(out_rows):
        print(f"{iid}: done already")
        return
    reuse = None
    if a.reuse:
        if a.contexts:
            raise SystemExit("--contexts needs BM25's context text, so it does not go with --reuse")
        if a.reuse == a.run:
            raise SystemExit("--reuse names this run itself")
        reuse = reused_task(work, a.reuse, iid)
        if reuse is None:
            raise SystemExit(f"{iid}: run {a.reuse} has no rows for this task")
    text = open(os.path.join(work, "texts", iid + ".txt"), encoding="utf-8").read()
    if hashlib.sha256(text.encode("utf-8")).hexdigest() != t["text_sha256"]:
        raise SystemExit(f"{iid}: the problem statement does not match text_sha256")
    t0 = time.time()
    src = checkout(work, scratch, t)
    data = os.path.join(scratch, "data", iid)
    info = {"id": iid, "repo": t["repo"], "base_commit": t["base_commit"], "graphicle": None}
    try:
        missing = [g for g in t["gold"] if git(src, "cat-file", "-e", "HEAD:" + g, check=False).returncode != 0]
        gold = [g for g in t["gold"] if g not in missing]
        info.update({"gold_n": len(gold), "gold_missing_at_base": missing, "checkout_seconds": round(time.time() - t0, 1)})
        if not gold:
            raise RuntimeError("no gold file exists at the base commit")
        if reuse:
            ri = reuse["info"]
            if ri.get("gold_n") != len(gold) or ri.get("gold_missing_at_base") != missing:
                raise RuntimeError(f"the gold files differ from run {a.reuse}'s")
            info.update({k: ri[k] for k in ("files", "tokens_all", "tokens_eligible", "counter")})
            info["reused"] = {"run": a.reuse, "arms": list(REUSED_ARMS), "graphicle": ri.get("graphicle")}
            elig = ri["tokens_eligible"]
            budgets = [(lb, b) for lb, b in BUDGETS + [BIG] if lb in ri["budgets"]]
            if [lb for lb, _b in budgets] != ri["budgets"]:
                raise RuntimeError(f"unknown budgets in run {a.reuse}: {ri['budgets']}")
        else:
            counter = Counter(a.tokenizer)
            t1 = time.time()
            rows_m = build_manifest(scratch, iid, counter)
            info["manifest_seconds"] = round(time.time() - t1, 1)
            elig = sum(c + w for _r, _p, c, w in truncation_order(iid, rows_m))
            info.update({"files": len(rows_m), "tokens_all": sum(c for _p, _cls, _b, c, _w in rows_m if c > 0),
                         "tokens_eligible": elig, "counter": counter.name})
            budgets = BUDGETS + ([BIG] if elig > BIG[1] else [])
        info["graphicle"] = subprocess.run([a.graphicle, "--version"], capture_output=True, text=True).stdout.strip()
        os.makedirs(data, exist_ok=True)
        code, out, err, wall, cpu, peak = measured([a.graphicle, "sync", src, "--data-dir", data, "--json"], 3600)
        if code != 0:
            raise RuntimeError("graphicle sync: " + err.strip()[-300:])
        info["index"] = {"wall_seconds": round(wall, 1), "cpu_seconds": round(cpu, 1), "peak_rss_mib": round(peak, 1),
                         "data_bytes": du_bytes(data)}
        rows, secs = [], {"pack": 0.0, "bm25": 0.0, "trunc": 0.0}
        cache = {}
        ctx_dir = os.path.join(ctx_root, iid)
        for label, b in budgets:
            fits = elig <= b
            s = time.time()
            code, out, err, _w, _c, peak = measured(
                pack_command(a.graphicle, src, data, text, b, a.tokenizer, "json", layout="standard"), 1800)
            if code != 0:
                raise RuntimeError("graphicle pack: " + err.strip()[-300:])
            pj = json.loads(out)
            secs["pack"] += time.time() - s
            if pj.get("layout") not in (None, "standard"):
                raise RuntimeError(f"pack layout {pj.get('layout')!r}, not standard")
            pinc = [f["path"] for f in pj.get("included") or []]
            whole = {c.get("path") for sec in pj.get("sections") or [] for c in sec.get("chunks") or [] if c.get("whole")}
            outline = outline_only(pj)
            r = row(t, "pack", label, b, pinc, pj.get("tokens"), time.time() - s, gold, fits)
            r.update({"inside_whole": sum(1 for g in gold if g in whole),
                      "inside_outline_only": sum(1 for g in gold if g in outline and g in pinc),
                      "peak_mib": round(peak, 1), "tokenizer": pj.get("tokenizer")})
            rows.append(r)
            if reuse:
                for arm in REUSED_ARMS:
                    old = reuse["rows"].get((arm, label))
                    if old is None or old["fits_whole"] != fits or old["gold_n"] != len(gold):
                        raise RuntimeError(f"run {a.reuse} has no matching {arm} row at {label}")
                    rows.append(old)
                continue
            s = time.time()
            btext, binc, btok = build_bm25(scratch, iid, text, b, counter, cache)
            secs["bm25"] += time.time() - s
            rows.append(row(t, "bm25", label, b, binc, btok, time.time() - s, gold, fits))
            s = time.time()
            _tt, tinc, ttok = build_truncation(scratch, iid, b, counter)
            secs["trunc"] += time.time() - s
            rows.append(row(t, "trunc", label, b, tinc, ttok, time.time() - s, gold, fits))
            if a.contexts and label == CONTEXT_BUDGET:
                os.makedirs(ctx_dir, exist_ok=True)
                code, xml, err, _w, _c, _p = measured(
                    pack_command(a.graphicle, src, data, text, b, a.tokenizer, "xml", layout="standard"), 1800)
                if code != 0:
                    raise RuntimeError("graphicle pack (xml): " + err.strip()[-300:])
                for fname, body, inc in (("pack-64K.xml", xml, pinc), ("bm25-64K.txt", btext, binc)):
                    with open(os.path.join(ctx_dir, fname), "w", encoding="utf-8") as f:
                        f.write(body)
                    info.setdefault("contexts", {})[fname] = {
                        "sha256": hashlib.sha256(body.encode("utf-8")).hexdigest(), "tokens": counter.count(body),
                        "files": len(inc)}
        info["arm_seconds"] = {k: round(v, 1) for k, v in secs.items() if not (reuse and k in REUSED_ARMS)}
        info["budgets"] = [lb for lb, _b in budgets]
        info["seconds"] = round(time.time() - t0, 1)
        os.makedirs(os.path.dirname(out_rows), exist_ok=True)
        os.makedirs(os.path.dirname(out_task), exist_ok=True)
        with open(out_task, "w") as f:
            json.dump(info, f, indent=1, sort_keys=True)
        with open(out_rows + ".tmp", "w") as f:
            for r in rows:
                f.write(json.dumps(r, sort_keys=True) + "\n")
        os.replace(out_rows + ".tmp", out_rows)
        print(f"{iid}: {len(rows)} rows, index {info['index']['wall_seconds']} s, total {info['seconds']} s", flush=True)
    except Exception as e:
        info["error"] = f"{type(e).__name__}: {str(e)[:400]}"
        os.makedirs(os.path.dirname(out_task), exist_ok=True)
        with open(out_task + ".failed", "w") as f:
            json.dump(info, f, indent=1, sort_keys=True)
        with open(out_task + ".attempts", "a") as f:  # one line per failed attempt
            f.write(json.dumps({"time": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "error": info["error"]}) + "\n")
        raise
    finally:
        if not a.keep:
            shutil.rmtree(src, ignore_errors=True)
            shutil.rmtree(data, ignore_errors=True)
            try:
                os.remove(os.path.join(scratch, "manifest", iid + ".tsv"))
            except OSError:
                pass


def cmd_run(a):
    work = os.path.abspath(a.work)
    tasks = load_tasks(work)
    if a.ids:
        d = json.load(open(a.ids))
        ids = d["ids"] if isinstance(d, dict) else d
    else:
        ids = sorted(i for i, t in tasks.items() if t["gold"])
    if a.order == "shuffled":
        ids = sorted(ids)
        random.Random(ORDER_SEED).shuffle(ids)
    if a.reuse:
        if a.contexts:
            raise SystemExit("--contexts needs BM25's context text, so it does not go with --reuse")
        _s, rdir, _c, _l = run_paths(work, a.reuse)
        have = [i for i in ids if os.path.exists(os.path.join(rdir, "rows", i + ".jsonl"))]
        print(f"{a.run}: the pack again; BM25 and truncation rows from {a.reuse}, which measured {len(have)} of "
              f"these {len(ids)} tasks", flush=True)
        ids = have
    _scratch, outdir, _ctx, logs = run_paths(work, a.run)

    def attempts(i):
        p = os.path.join(outdir, "tasks", i + ".json.attempts")
        return sum(1 for _ in open(p)) if os.path.exists(p) else 0

    # A task is tried at most twice; one that failed twice is reported as failed.
    todo = [i for i in ids if not os.path.exists(os.path.join(outdir, "rows", i + ".jsonl"))
            and attempts(i) < MAX_ATTEMPTS]
    print(f"{a.run}: {len(ids)} tasks, {len(todo)} to run, {a.jobs} at once", flush=True)
    os.makedirs(logs, exist_ok=True)
    env = dict(os.environ, RAYON_NUM_THREADS=str(a.threads), TOKENIZERS_PARALLELISM="true")

    def one(iid):
        cmd = [sys.executable, os.path.abspath(__file__), "task", "--work", work, "--graphicle", a.graphicle,
               "--tokenizer", a.tokenizer, "--id", iid, "--run", a.run] + (["--contexts"] if a.contexts else []) + \
              (["--keep"] if a.keep else []) + (["--reuse", a.reuse] if a.reuse else [])
        with open(os.path.join(logs, iid + ".log"), "w") as log:
            r = subprocess.run(cmd, stdout=log, stderr=subprocess.STDOUT, env=env)
        print(f"{time.strftime('%H:%M:%S')} {iid}: exit {r.returncode}", flush=True)
        return r.returncode

    with ThreadPoolExecutor(a.jobs) as ex:
        codes = list(ex.map(one, todo))
    print(f"done: {sum(1 for c in codes if c == 0)} ok, {sum(1 for c in codes if c != 0)} failed", flush=True)
    again = [i for i in todo if not os.path.exists(os.path.join(outdir, "rows", i + ".jsonl"))
             and attempts(i) < MAX_ATTEMPTS]
    if again:
        print(f"trying {len(again)} failed tasks once more", flush=True)
        with ThreadPoolExecutor(a.jobs) as ex:
            codes = list(ex.map(one, again))
        print(f"second try: {sum(1 for c in codes if c == 0)} ok, {sum(1 for c in codes if c != 0)} failed", flush=True)


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    for name in ("run", "task"):
        p = sub.add_parser(name)
        p.add_argument("--work", required=True)
        p.add_argument("--graphicle", required=True)
        p.add_argument("--tokenizer", required=True)
        p.add_argument("--contexts", action="store_true")
        p.add_argument("--keep", action="store_true")
        p.add_argument("--run", default="e1", help="the run's name: e1 (the first build) keeps the original "
                                                  "layout, another name gets its own folders (run_paths)")
        p.add_argument("--reuse", help="an earlier run: measure only the pack, and take that run's BM25 and "
                                       "truncation rows, budgets and token counts")
        if name == "run":
            p.add_argument("--ids")
            p.add_argument("--order", choices=["sample", "shuffled"], default="sample")
            p.add_argument("--jobs", type=int, default=6)
            p.add_argument("--threads", type=int, default=6)
        else:
            p.add_argument("--id", required=True)
    a = ap.parse_args()
    {"run": cmd_run, "task": cmd_task}[a.cmd](a)


if __name__ == "__main__":
    main()
