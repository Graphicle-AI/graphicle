#!/usr/bin/env python3
"""E1L: line-level recall on SWE-bench Pro, no model: do the contexts hold the lines each fix changes?

usage:
  e1l.py gold --work WORK --parquet FILE
      writes WORK/e1l/gold.jsonl: for every task in WORK/tasks.jsonl with gold files, each gold file's gold lines,
      the pre-image line numbers of the reference patch: its '-' lines; for a hunk without '-' lines, the
      pre-image line just before each run of '+' lines (line 1 when the run opens the file). A gold file whose
      section has no hunk (a pure rename, a mode change, a binary file) has no gold lines.
  e1l.py run --work WORK --graphicle BIN --tokenizer T [--run e1l] [--jobs 6] [--threads 6]
      every task in gold.jsonl that has no rows yet, in e1.py's shuffled order, each in its own process
  e1l.py task --work WORK --graphicle BIN --tokenizer T --id ID [--run e1l]
      one task: check out the base commit (WORK/mirror), check every gold line against the file there, count
      tokens (tokens.py), build the index, then at each of E1's budgets write what each context holds of each
      gold file to WORK/out/RUN/rows/ID.jsonl, one line per arm and budget:
      - pack (`graphicle pack --format json`, standard layout, as e1.py): the line ranges of its text chunks
        (start_line-end_line: whole files and excerpts, every line between them verbatim), the gold lines it
        shows only in an outline (the numbered lines of a chunk whose `why` says outline), whether one chunk
        holds the file whole, and the file's rank and tokens in `included`, or its rank in `omitted` (the
        top 50 ranked files left out);
      - BM25 and truncation (whole files only): whether the file is in, and BM25's rank of it.
      A gold line is inside the pack when a text chunk's range covers it; outline lines never count as inside.
  --reuse RUN (run and task) measures the pack again with another build and takes the BM25 and truncation rows of
      the earlier E1L run RUN as they are (those arms are this kit's own code: a build cannot change them), with the
      task's budgets, eligible tokens and gold file sizes from RUN's record of it (no tokenizer pass); the index is
      built again by BIN, every gold line is checked again, and the record notes the reuse. Only tasks RUN measured
      are run.
"""
import argparse
import hashlib
import json
import os
import random
import re
import shutil
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import e1  # noqa: E402  (also puts ../whole-repo on the path)
from bm25 import build_bm25  # noqa: E402
from mine import parse_diff  # noqa: E402
from pack_arm import pack_command  # noqa: E402
from tokens import Counter, build_manifest, build_truncation, truncation_order  # noqa: E402

HUNK = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")
OUTLINE_LINE = re.compile(r"^(\d+)\t")


def file_sections(patch):
    """[(old path, status, lines)] of a git diff, split on '\\n' only (old, new and status as data.py reads them)."""
    out, cur = [], None
    for line in patch.split("\n"):
        if line.startswith("diff --git "):
            cur = [line]
            out.append(cur)
        elif cur is not None:
            cur.append(line)
    res = []
    for lines in out:
        s = parse_diff("\n".join(lines) + "\n")[0]
        res.append((s["old"], s["status"], lines))
    return res


def gold_lines(lines):
    """(sorted gold line numbers, {line: text of its '-' line}, hunks) of one file section."""
    gold, minus_text, hunks, i, n = set(), {}, 0, 0, len(lines)
    while i < n:
        m = HUNK.match(lines[i])
        if not m:
            i += 1
            continue
        hunks += 1
        a, alen = int(m.group(1)), 1 if m.group(2) is None else int(m.group(2))
        blen = 1 if m.group(4) is None else int(m.group(4))
        cur = a if alen > 0 else a + 1  # the next pre-image line
        old_left, new_left, body = alen, blen, []
        i += 1
        while i < n and (old_left > 0 or new_left > 0):
            line = lines[i]
            if line.startswith("\\"):  # "\ No newline at end of file"
                i += 1
                continue
            tag = line[:1]
            if tag == " " or line == "":
                body.append((" ", cur, None))
                cur, old_left, new_left = cur + 1, old_left - 1, new_left - 1
            elif tag == "-":
                body.append(("-", cur, line[1:]))
                cur, old_left = cur + 1, old_left - 1
            elif tag == "+":
                body.append(("+", cur, None))
                new_left -= 1
            else:
                break
            i += 1
        minus = [(ln, t) for tag, ln, t in body if tag == "-"]
        if minus:
            for ln, t in minus:
                gold.add(ln)
                minus_text[ln] = t
        else:
            prev = None
            for tag, ln, _t in body:
                if tag == "+" and prev != "+":
                    gold.add(max(ln - 1, 1))
                prev = tag
    return sorted(gold), minus_text, hunks


def cmd_gold(a):
    import pyarrow.parquet as pq
    work = os.path.abspath(a.work)
    tasks = e1.load_tasks(work)
    rows = {r["instance_id"]: r for r in pq.read_table(a.parquet, columns=["instance_id", "patch"]).to_pylist()}
    os.makedirs(os.path.join(work, "e1l"), exist_ok=True)
    n_files = n_lines = no_lines = 0
    with open(os.path.join(work, "e1l", "gold.jsonl"), "w") as f:
        for iid in sorted(tasks):
            t = tasks[iid]
            if not t["gold"]:
                continue
            files = {}
            for old, status, lines in file_sections(rows[iid]["patch"] or ""):
                if old not in t["gold"] or status == "added":
                    continue
                g, minus, hunks = gold_lines(lines)
                prev = files.get(old)
                if prev:  # a path in two sections: join them
                    g = sorted(set(prev["lines"]) | set(g))
                    minus = {**{int(k): v for k, v in prev["minus"].items()}, **minus}
                    hunks += prev["hunks"]
                files[old] = {"lines": g, "minus": {str(k): v for k, v in minus.items()}, "hunks": hunks,
                              "status": status}
            for p in t["gold"]:
                files.setdefault(p, {"lines": [], "minus": {}, "hunks": 0, "status": "missing"})
            n_files += len(files)
            n_lines += sum(len(v["lines"]) for v in files.values())
            no_lines += sum(1 for v in files.values() if not v["lines"])
            f.write(json.dumps({"id": iid, "files": files}, sort_keys=True) + "\n")
    print(f"gold.jsonl: {n_files} gold files, {n_lines} gold lines, {no_lines} files without gold lines")


def file_lines(path):
    """The file's lines as `str::lines()` splits them: on '\\n', one trailing '\\r' dropped, no last empty line."""
    with open(path, "rb") as fh:
        text = fh.read().decode("utf-8", "replace")
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    return [ln[:-1] if ln.endswith("\r") else ln for ln in lines], len(text.encode("utf-8"))


def merge(ranges):
    out = []
    for s, e in sorted(ranges):
        if out and s <= out[-1][1] + 1:
            out[-1][1] = max(out[-1][1], e)
        else:
            out.append([s, e])
    return out


def covered(line, ranges):
    return any(s <= line <= e for s, e in ranges)


def pack_view(pj, gold):
    """{gold path: text ranges (merged), outline lines, whole} of one pack's JSON."""
    view = {p: {"ranges": [], "outline": set(), "whole": False} for p in gold}
    for sec in pj.get("sections") or []:
        for c in sec.get("chunks") or []:
            p = c.get("path")
            if p not in view:
                continue
            if not c.get("whole") and "(outline" in (c.get("why") or ""):
                for ln in (c.get("text") or "").split("\n"):
                    m = OUTLINE_LINE.match(ln)
                    if m:
                        view[p]["outline"].add(int(m.group(1)))
            else:
                view[p]["ranges"].append((int(c["start_line"]), int(c["end_line"])))
                view[p]["whole"] = view[p]["whole"] or bool(c.get("whole"))
    for v in view.values():
        v["ranges"] = merge(v["ranges"])
    return view


def summarize(t, arm, label, budget, tokens, nfiles, per_file):
    have = [f for f in per_file if f["n_lines"]]
    total = sum(f["n_lines"] for f in have)
    inside = sum(f["inside_lines"] for f in have)
    return {"id": t["instance_id"], "repo": t["repo"], "arm": arm, "budget": label, "budget_tokens": budget,
            "context_tokens": tokens, "context_files": nfiles, "gold_n": len(per_file),
            "file_inside": sum(1 for f in per_file if f["in_context"]),
            "gold_lines": total, "lines_inside": inside, "line_share": inside / total if total else None,
            "all_lines": int(inside == total) if total else None,
            "files_all_lines": sum(1 for f in have if f["inside_lines"] == f["n_lines"]), "files_with_lines": len(have),
            "names_gold_path": t["names_gold_path"], "files": per_file}


def cmd_task(a):
    work = os.path.abspath(a.work)
    t = e1.load_tasks(work)[a.id]
    iid = t["instance_id"]
    gold_rec = next(r for r in map(json.loads, open(os.path.join(work, "e1l", "gold.jsonl"))) if r["id"] == iid)
    scratch, outdir, _ctx, _logs = e1.run_paths(work, a.run)
    out_rows = os.path.join(outdir, "rows", iid + ".jsonl")
    out_task = os.path.join(outdir, "tasks", iid + ".json")
    if os.path.exists(out_rows):
        print(f"{iid}: done already")
        return
    text = open(os.path.join(work, "texts", iid + ".txt"), encoding="utf-8").read()
    if hashlib.sha256(text.encode("utf-8")).hexdigest() != t["text_sha256"]:
        raise SystemExit(f"{iid}: the problem statement does not match text_sha256")
    t0 = time.time()
    src = e1.checkout(work, scratch, t)
    data = os.path.join(scratch, "data", iid)
    info = {"id": iid, "repo": t["repo"], "base_commit": t["base_commit"], "graphicle": None}
    try:
        missing = [g for g in t["gold"] if e1.git(src, "cat-file", "-e", "HEAD:" + g, check=False).returncode != 0]
        gold = [g for g in t["gold"] if g not in missing]
        if not gold:
            raise RuntimeError("no gold file exists at the base commit")
        reused = None
        if a.reuse:
            _rs, rdir, _rc, _rl = e1.run_paths(work, a.reuse)
            rec = json.load(open(os.path.join(rdir, "tasks", iid + ".json")))
            reused = {(r["arm"], r["budget"]): r
                      for r in map(json.loads, open(os.path.join(rdir, "rows", iid + ".jsonl")))
                      if r["arm"] in ("bm25", "trunc")}
            label_of = dict(e1.BUDGETS + [e1.BIG])
            budgets = [(lb, label_of[lb]) for lb in rec["budgets"]]
            elig = rec["tokens_eligible"]
            tok_of = {g: (rec["sizes"].get(g) or {}).get("tokens") for g in gold}
            counter_name = rec["counter"]
        else:
            counter = Counter(a.tokenizer)
            rows_m = build_manifest(scratch, iid, counter)
            tok_of = {p: c for p, _cls, _b, c, _w in rows_m}
            elig = sum(c + w for _r, _p, c, w in truncation_order(iid, rows_m))
            budgets = e1.BUDGETS + ([e1.BIG] if elig > e1.BIG[1] else [])
            counter_name = counter.name
        sizes, check = {}, {"minus_checked": 0, "minus_mismatch": [], "point_out_of_range": []}
        for g in gold:
            lines, nbytes = file_lines(os.path.join(src, g))
            rec = gold_rec["files"][g]
            for k, txt in rec["minus"].items():
                ln = int(k)
                check["minus_checked"] += 1
                want = txt[:-1] if txt.endswith("\r") else txt
                if ln > len(lines) or lines[ln - 1] != want:
                    check["minus_mismatch"].append([g, ln])
            for ln in rec["lines"]:
                if str(ln) not in rec["minus"] and not 1 <= ln <= max(len(lines), 1):
                    check["point_out_of_range"].append([g, ln])
            sizes[g] = {"tokens": tok_of.get(g), "bytes": nbytes, "lines": len(lines),
                        "minified": bool(lines) and nbytes // len(lines) >= 300}
        info.update({"gold_n": len(gold), "gold_missing_at_base": missing, "budgets": [lb for lb, _b in budgets],
                     "tokens_eligible": elig, "sizes": sizes, "check": check, "counter": counter_name,
                     "reuse": a.reuse})
        info["graphicle"] = subprocess.run([a.graphicle, "--version"], capture_output=True, text=True).stdout.strip()
        os.makedirs(data, exist_ok=True)
        code, _out, err, wall, cpu, peak = e1.measured([a.graphicle, "sync", src, "--data-dir", data, "--json"], 3600)
        if code != 0:
            raise RuntimeError("graphicle sync: " + err.strip()[-300:])
        info["index"] = {"wall_seconds": round(wall, 1), "cpu_seconds": round(cpu, 1), "peak_rss_mib": round(peak, 1)}
        rows, cache, bm25_rank = [], {}, None
        for label, b in budgets:
            code, out, err, _w, _c, _p = e1.measured(
                pack_command(a.graphicle, src, data, text, b, a.tokenizer, "json", layout="standard"), 1800)
            if code != 0:
                raise RuntimeError("graphicle pack: " + err.strip()[-300:])
            pj = json.loads(out)
            view = pack_view(pj, gold)
            inc = {f["path"]: f for f in pj.get("included") or []}
            om = {f["path"]: f for f in pj.get("omitted") or []}
            per = []
            for g in gold:
                gl, v = gold_rec["files"][g]["lines"], view[g]
                ins = [ln for ln in gl if covered(ln, v["ranges"])]
                f = {"path": g, "n_lines": len(gl), "inside_lines": len(ins),
                     "outline_lines": sum(1 for ln in gl if ln in v["outline"] and ln not in ins),
                     "in_context": g in inc, "whole": v["whole"], "ranges": v["ranges"],
                     "text": bool(v["ranges"]), "outline": bool(v["outline"]),
                     "rank": (inc.get(g) or om.get(g) or {}).get("rank"),
                     "pack_tokens": (inc.get(g) or {}).get("tokens"), "omitted_listed": g in om}
                if v["ranges"] and len(ins) < len(gl):
                    f["min_dist"] = min(min(abs(ln - s), abs(ln - e)) for ln in gl if ln not in ins
                                        for s, e in v["ranges"])
                per.append(f)
            rows.append(summarize(t, "pack", label, b, pj.get("tokens"), len(inc), per))
            if reused is not None:
                for arm in ("bm25", "trunc"):
                    r = reused.get((arm, label))
                    if r is None:
                        raise RuntimeError(f"{a.reuse} has no {arm} row at {label}")
                    rows.append(r)
                continue
            _bt, binc, btok = build_bm25(scratch, iid, text, b, counter, cache)
            if bm25_rank is None:
                bm25_rank = {p: i + 1 for i, (p, _s) in enumerate(next(iter(cache.values()))[0])}
            bset = set(binc)
            per = [{"path": g, "n_lines": len(gold_rec["files"][g]["lines"]),
                    "inside_lines": len(gold_rec["files"][g]["lines"]) if g in bset else 0,
                    "in_context": g in bset, "whole": g in bset, "rank": bm25_rank.get(g)} for g in gold]
            rows.append(summarize(t, "bm25", label, b, btok, len(binc), per))
            _tt, tinc, ttok = build_truncation(scratch, iid, b, counter)
            tset = set(tinc)
            per = [{"path": g, "n_lines": len(gold_rec["files"][g]["lines"]),
                    "inside_lines": len(gold_rec["files"][g]["lines"]) if g in tset else 0,
                    "in_context": g in tset, "whole": g in tset} for g in gold]
            rows.append(summarize(t, "trunc", label, b, ttok, len(tinc), per))
        info["seconds"] = round(time.time() - t0, 1)
        os.makedirs(os.path.dirname(out_rows), exist_ok=True)
        os.makedirs(os.path.dirname(out_task), exist_ok=True)
        with open(out_task, "w") as fh:
            json.dump(info, fh, indent=1, sort_keys=True)
        with open(out_rows + ".tmp", "w") as fh:
            for r in rows:
                fh.write(json.dumps(r, sort_keys=True) + "\n")
        os.replace(out_rows + ".tmp", out_rows)
        print(f"{iid}: {len(rows)} rows, total {info['seconds']} s", flush=True)
    except Exception as ex:
        info["error"] = f"{type(ex).__name__}: {str(ex)[:400]}"
        os.makedirs(os.path.dirname(out_task), exist_ok=True)
        with open(out_task + ".failed", "w") as fh:
            json.dump(info, fh, indent=1, sort_keys=True)
        with open(out_task + ".attempts", "a") as fh:
            fh.write(json.dumps({"time": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "error": info["error"]})
                     + "\n")
        raise
    finally:
        shutil.rmtree(src, ignore_errors=True)
        shutil.rmtree(data, ignore_errors=True)
        try:
            os.remove(os.path.join(scratch, "manifest", iid + ".tsv"))
        except OSError:
            pass


def cmd_run(a):
    work = os.path.abspath(a.work)
    ids = sorted(r["id"] for r in map(json.loads, open(os.path.join(work, "e1l", "gold.jsonl"))))
    if a.ids:
        d = json.load(open(a.ids))
        keep = set(d["ids"] if isinstance(d, dict) else d)
        ids = [i for i in ids if i in keep]
    if a.reuse:
        _rs, rdir, _rc, _rl = e1.run_paths(work, a.reuse)
        ids = [i for i in ids if os.path.exists(os.path.join(rdir, "rows", i + ".jsonl"))]
    random.Random(e1.ORDER_SEED).shuffle(ids)
    _s, outdir, _c, logs = e1.run_paths(work, a.run)

    def attempts(i):
        p = os.path.join(outdir, "tasks", i + ".json.attempts")
        return sum(1 for _ in open(p)) if os.path.exists(p) else 0

    def todo_of(ids_):
        return [i for i in ids_ if not os.path.exists(os.path.join(outdir, "rows", i + ".jsonl"))
                and attempts(i) < e1.MAX_ATTEMPTS]
    todo = todo_of(ids)
    print(f"{a.run}: {len(ids)} tasks, {len(todo)} to run, {a.jobs} at once", flush=True)
    os.makedirs(logs, exist_ok=True)
    env = dict(os.environ, RAYON_NUM_THREADS=str(a.threads), TOKENIZERS_PARALLELISM="true")

    def one(iid):
        cmd = [sys.executable, os.path.abspath(__file__), "task", "--work", work, "--graphicle", a.graphicle,
               "--tokenizer", a.tokenizer, "--id", iid, "--run", a.run] + (["--reuse", a.reuse] if a.reuse else [])
        with open(os.path.join(logs, iid + ".log"), "w") as log:
            r = subprocess.run(cmd, stdout=log, stderr=subprocess.STDOUT, env=env)
        print(f"{time.strftime('%H:%M:%S')} {iid}: exit {r.returncode}", flush=True)
        return r.returncode
    for attempt in (1, 2):
        if attempt == 2:
            todo = todo_of(todo)
            if not todo:
                break
            print(f"trying {len(todo)} failed tasks once more", flush=True)
        with ThreadPoolExecutor(a.jobs) as ex:
            codes = list(ex.map(one, todo))
        print(f"pass {attempt}: {sum(1 for c in codes if c == 0)} ok, {sum(1 for c in codes if c != 0)} failed",
              flush=True)


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    g = sub.add_parser("gold")
    g.add_argument("--work", required=True)
    g.add_argument("--parquet", required=True)
    for name in ("run", "task"):
        p = sub.add_parser(name)
        p.add_argument("--work", required=True)
        p.add_argument("--graphicle", required=True)
        p.add_argument("--tokenizer", required=True)
        p.add_argument("--run", default="e1l")
        p.add_argument("--reuse")
        if name == "run":
            p.add_argument("--ids")
            p.add_argument("--jobs", type=int, default=6)
            p.add_argument("--threads", type=int, default=6)
        else:
            p.add_argument("--id", required=True)
    a = ap.parse_args()
    {"gold": cmd_gold, "run": cmd_run, "task": cmd_task}[a.cmd](a)


if __name__ == "__main__":
    main()
