#!/usr/bin/env python3
"""Build the change-localization question sets from merged pull requests.

usage:
  mine.py candidates --work WORKDIR --raw RAWDIR       pull requests from fetch_prs.sh, first filters
  mine.py diffs --work WORKDIR [--jobs 4]              each remaining pull request's diff from github.com
  mine.py check --work WORKDIR                         gold files from the diff; the diff must apply at S
  mine.py select --work WORKDIR --raw RAWDIR --out DIR issue texts (fetch_issues.sh), selection
Every command takes --repos a,b to work on those repositories only; the others' files are left as
they are (select keeps their rows in an existing funnel.json). The selection seed of a repository
depends on its place in repos.json, not on --repos.

WORKDIR holds the checkouts (checkout.py) and mine/, the intermediate files and the question texts,
which are not published. --out gets questions-<name>.jsonl and funnel.json.

The filters, in order (funnel.json counts what each one removed):
  window     landed on the default branch after S and by repos.json's merged_until
  linked     closes an issue of the same repository: GitHub's closing references, or a closing
             keyword in the body (close, closes, closed, fix, fixes, fixed, resolve, resolves,
             resolved, then #N or the issue's URL)
  files      at most 100 changed files
  prefilter  1 to 5 changed source files by GitHub's file list (paths.py), before the diff is read
  diff       the diff can be read
  gold       1 to 5 gold files: source files (paths.py) that exist at S and that the diff modifies,
             deletes or renames (new files are not gold)
  applies    the diff restricted to the gold files applies to S (git apply --check)
  issue      a linked issue is an issue (not a pull request) created before the pull request landed;
             the earliest such issue is the question
  duplicate  the issue was not already the question of an earlier pull request
Then every pull request left is kept if there are at most `target` of them (repos.json); otherwise
`target` are drawn with random.Random(selection_seed + index of the repository). Kept questions are
listed oldest first.
"""
import argparse
import datetime
import glob
import json
import os
import random
import re
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from paths import classify  # noqa: E402
from issue_text import question_text, sha256  # noqa: E402

CFG = json.load(open(os.path.join(HERE, "repos.json")))
SNAP = json.load(open(os.path.join(HERE, "snapshots.json")))
KEYWORD = r"(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)"


def ts(s):
    return datetime.datetime.fromisoformat(s.replace("Z", "+00:00"))


def read_pages(path):
    text, dec, pos, out = open(path).read(), json.JSONDecoder(), 0, []
    while True:
        while pos < len(text) and text[pos].isspace():
            pos += 1
        if pos >= len(text):
            return out
        obj, pos = dec.raw_decode(text, pos)
        out.append(obj)


def mdir(work, *parts):
    d = os.path.join(work, "mine", *parts)
    os.makedirs(d, exist_ok=True)
    return d


def tracked(work, name):
    out = subprocess.run(["git", "ls-files", "-z"], cwd=os.path.join(work, "src", name),
                         capture_output=True, text=True, check=True).stdout
    return set(p for p in out.split("\0") if p)


def linked_issues(repo, pr):
    owner_repo = repo["slug"]
    nums = []
    for n in (pr.get("closingIssuesReferences") or {}).get("nodes") or []:
        if n and n["repository"]["nameWithOwner"].lower() == owner_repo.lower():
            nums.append(n["number"])
    body = COMMENT_RX.sub("", pr.get("body") or "")
    rx = re.compile(r"\b" + KEYWORD + r":?\s+(?:(?:" + re.escape(owner_repo) + r")?#(\d+)|https://github\.com/"
                    + re.escape(owner_repo) + r"/issues/(\d+))\b", re.I)
    for m in rx.finditer(body):
        nums.append(int(m.group(1) or m.group(2)))
    seen, out = set(), []
    for n in nums:
        if n not in seen and n != pr["number"]:
            seen.add(n)
            out.append(n)
    return out


COMMENT_RX = re.compile(r"<!--.*?-->", re.S)


def chosen(a):
    """The repositories of repos.json that --repos names (all of them without --repos), with their index."""
    return [(i, r) for i, r in enumerate(CFG["repos"]) if not a.repos or r["name"] in a.repos]


def cmd_candidates(a):
    until = ts(CFG["merged_until"])
    for _ri, repo in chosen(a):
        name = repo["name"]
        s_time = ts(SNAP[name]["committed"])
        prs = {}
        for path in sorted(glob.glob(os.path.join(a.raw, f"prs-{name}*.json"))):
            for page in read_pages(path):
                for pr in page["data"]["search"]["nodes"]:
                    if pr and "number" in pr:
                        prs[pr["number"]] = pr
        rows = []
        for num in sorted(prs):
            pr = prs[num]
            labels = {l["name"] for l in (pr.get("labels") or {}).get("nodes") or []}
            landed = pr["mergedAt"] if pr.get("merged") else (pr["closedAt"] if "Merged" in labels else None)
            row = {"pr": num, "url": pr["url"], "landed_at": landed, "created_at": pr["createdAt"],
                   "base": pr["baseRefName"], "issues": linked_issues(repo, pr), "drop": None}
            base_ok = pr["baseRefName"] == repo["branch"] or (name == "pytorch" and "Merged" in labels
                                                             and re.match(r"^gh/[^/]+/\d+/base$", pr["baseRefName"]))
            files = pr.get("files") or {}
            nsrc = sum(1 for f in files.get("nodes") or []
                       if f["changeType"] != "ADDED" and classify(name, f["path"]) == "source")
            if not landed or not base_ok or not (s_time < ts(landed) <= until):
                row["drop"] = "window"
            elif not row["issues"]:
                row["drop"] = "linked"
            elif (files.get("totalCount") or 0) > 100:
                row["drop"] = "files"
            elif not 1 <= nsrc <= 5:
                row["drop"] = "prefilter"
            rows.append(row)
        with open(os.path.join(mdir(a.work), f"candidates-{name}.json"), "w") as f:
            json.dump(rows, f, indent=0)
        left = sum(1 for r in rows if not r["drop"])
        print(f"{name}: {len(rows)} pull requests, {left} need their diff", flush=True)


def fetch_diff(slug, num, dest):
    if os.path.exists(dest):
        return "cached"
    url = f"https://github.com/{slug}/pull/{num}.diff"
    for attempt in range(6):
        try:
            req = urllib.request.Request(url, headers={"User-Agent": "graphicle-bench-whole-repo"})
            with urllib.request.urlopen(req, timeout=60) as r:
                data = r.read()
            if data and not data.startswith(b"diff --git"):
                return "not-a-diff"
            with open(dest + ".part", "wb") as f:
                f.write(data)
            os.replace(dest + ".part", dest)
            return "ok"
        except urllib.error.HTTPError as e:
            if e.code in (429, 500, 502, 503, 504):
                time.sleep(20 * (attempt + 1))
                continue
            return f"http-{e.code}"
        except (urllib.error.URLError, TimeoutError, ConnectionError):
            time.sleep(10 * (attempt + 1))
    return "failed"


def cmd_diffs(a):
    for _ri, repo in chosen(a):
        name = repo["name"]
        rows = json.load(open(os.path.join(mdir(a.work), f"candidates-{name}.json")))
        d = mdir(a.work, "diffs", name)
        todo = [r["pr"] for r in rows if not r["drop"]]
        with ThreadPoolExecutor(a.jobs) as ex:
            res = list(ex.map(lambda n: fetch_diff(repo["slug"], n, os.path.join(d, f"{n}.diff")), todo))
        counts = {}
        for x in res:
            counts[x] = counts.get(x, 0) + 1
        with open(os.path.join(d, "fetch-status.json"), "w") as f:
            json.dump(dict(zip(map(str, todo), res)), f)
        print(f"{name}: {counts}", flush=True)


def unquote(p):
    if p.startswith('"') and p.endswith('"'):
        p = bytes(p[1:-1], "utf-8").decode("unicode_escape").encode("latin-1").decode("utf-8", "replace")
    return p


def parse_diff(text):
    """Split a git diff into file sections: dicts with old, new, status and the section's text."""
    sections, cur = [], None
    for line in text.splitlines(keepends=True):
        if line.startswith("diff --git "):
            if cur:
                sections.append(cur)
            cur = {"lines": [line], "old": None, "new": None, "status": "modified"}
            hdr = line[len("diff --git "):].rstrip("\n")
            if hdr.startswith("a/") and " b/" in hdr:
                n = (len(hdr) - 5) // 2  # "a/X b/X": both halves name the same path
                if (len(hdr) - 5) % 2 == 0 and hdr[2 + n:5 + n] == " b/" and hdr[2:2 + n] == hdr[5 + n:]:
                    cur["old"] = cur["new"] = hdr[2:2 + n]
                else:
                    i = hdr.index(" b/")
                    cur["old"], cur["new"] = hdr[2:i], hdr[i + 3:]
            continue
        if cur is None:
            continue
        cur["lines"].append(line)
        if line.startswith("new file mode"):
            cur["status"] = "added"
        elif line.startswith("deleted file mode"):
            cur["status"] = "deleted"
        elif line.startswith("rename from "):
            cur["status"], cur["old"] = "renamed", unquote(line[len("rename from "):].rstrip("\n"))
        elif line.startswith("rename to "):
            cur["new"] = unquote(line[len("rename to "):].rstrip("\n"))
        elif line.startswith("--- ") and not line.startswith("--- /dev/null"):
            p = unquote(line[4:].rstrip("\n").split("\t")[0])
            cur["old"] = p[2:] if p.startswith("a/") else p
        elif line.startswith("+++ ") and not line.startswith("+++ /dev/null"):
            p = unquote(line[4:].rstrip("\n").split("\t")[0])
            cur["new"] = p[2:] if p.startswith("b/") else p
    if cur:
        sections.append(cur)
    return sections


def cmd_check(a):
    for _ri, repo in chosen(a):
        name = repo["name"]
        cpath = os.path.join(mdir(a.work), f"candidates-{name}.json")
        rows = json.load(open(cpath))
        at_s = tracked(a.work, name)
        src = os.path.join(a.work, "src", name)
        d = mdir(a.work, "diffs", name)
        need = []
        for r in rows:
            if r["drop"]:
                continue
            path = os.path.join(d, f"{r['pr']}.diff")
            if not os.path.exists(path):
                r["drop"] = "diff"
                continue
            secs = parse_diff(open(path, encoding="utf-8", errors="replace").read())
            if not secs:
                r["drop"] = "diff"
                continue
            gold = []
            for s in secs:
                old = s["old"]
                if s["status"] == "added" or not old or old not in at_s:
                    continue
                if classify(name, old, at_s) == "source":
                    gold.append(s)
            r["gold"] = sorted({s["old"] for s in gold})
            if not 1 <= len(r["gold"]) <= 5:
                r["drop"] = "gold"
                continue
            with tempfile.NamedTemporaryFile("w", suffix=".diff", delete=False) as f:
                for s in gold:
                    f.write("".join(s["lines"]))
                patch = f.name
            p = subprocess.run(["git", "apply", "--check", patch], cwd=src, capture_output=True, text=True)
            os.unlink(patch)
            if p.returncode != 0:
                r["drop"] = "applies"
                r["apply_error"] = p.stderr.strip().splitlines()[0][:200] if p.stderr.strip() else ""
                continue
            need.extend(r["issues"])
        with open(cpath, "w") as f:
            json.dump(rows, f, indent=0)
        with open(os.path.join(mdir(a.work), f"issues-needed-{name}.txt"), "w") as f:
            f.write("".join(f"{n}\n" for n in sorted(set(need))))
        left = sum(1 for r in rows if not r["drop"])
        print(f"{name}: {left} pull requests pass the diff checks; {len(set(need))} issues to fetch", flush=True)


def load_issues(raw, name):
    issues = {}
    for path in sorted(glob.glob(os.path.join(raw, f"issues-{name}-*.json"))):
        for page in read_pages(path):
            repo = (page.get("data") or {}).get("repository") or {}
            for k, v in repo.items():
                if v:
                    issues[int(k[1:])] = v
    return issues


def names_gold_path(text, gold):
    """True when the text names a gold file by a path of at least two components (dir/file)."""
    for g in gold:
        parts = g.split("/")
        for i in range(0, len(parts) - 1):
            if "/".join(parts[i:]) in text:
                return True
    return False


def cmd_select(a):
    os.makedirs(a.out, exist_ok=True)
    fpath = os.path.join(a.out, "funnel.json")
    funnel = json.load(open(fpath)) if a.repos and os.path.exists(fpath) else {}
    for ri, repo in chosen(a):
        name = repo["name"]
        rows = json.load(open(os.path.join(mdir(a.work), f"candidates-{name}.json")))
        issues = load_issues(a.raw, name)
        used, pool, texts = set(), [], {}
        for r in sorted(rows, key=lambda r: (r["landed_at"] or "", r["pr"])):
            if r["drop"]:
                continue
            choices = []
            for n in r["issues"]:
                iss = issues.get(n)
                if not iss or iss.get("__typename") != "Issue" or ts(iss["createdAt"]) >= ts(r["landed_at"]):
                    continue
                choices.append(iss)
            if not choices:
                r["drop"] = "issue"
                continue
            iss = min(choices, key=lambda i: (i["createdAt"], i["number"]))
            if iss["number"] in used:
                r["drop"] = "duplicate"
                continue
            used.add(iss["number"])
            text, notes = question_text(iss)
            q = {"id": f"{name}-{r['pr']}", "repo": repo["slug"], "S": SNAP[name]["sha"],
                 "issue_url": iss["url"], "pr_url": r["url"], "merged_at": r["landed_at"],
                 "gold": r["gold"], "text_sha256": sha256(text), "names_gold_path": names_gold_path(text, r["gold"])}
            pool.append(q)
            texts[q["id"]] = {"id": q["id"], "text": text, "notes": notes, "issue_created_at": iss["createdAt"]}
        target = repo["target"]
        if len(pool) > target:
            keep = random.Random(CFG["selection_seed"] + ri).sample(pool, target)
            keep.sort(key=lambda q: (q["merged_at"], q["id"]))
        else:
            keep = pool
        with open(os.path.join(a.out, f"questions-{name}.jsonl"), "w") as f:
            for q in keep:
                f.write(json.dumps(q, sort_keys=True) + "\n")
        with open(os.path.join(mdir(a.work), f"texts-{name}.jsonl"), "w") as f:
            for q in keep:
                f.write(json.dumps(texts[q["id"]], sort_keys=True) + "\n")
        counts = {"fetched": len(rows)}
        for k in ["window", "linked", "files", "prefilter", "diff", "gold", "applies", "issue", "duplicate"]:
            counts["dropped_" + k] = sum(1 for r in rows if r["drop"] == k)
        counts["passed"] = len(pool)
        counts["kept"] = len(keep)
        counts["sampled"] = len(pool) > target
        counts["names_gold_path"] = sum(1 for q in keep if q["names_gold_path"])
        funnel[name] = counts
        print(f"{name}: {json.dumps(counts)}", flush=True)
        with open(os.path.join(mdir(a.work), f"candidates-{name}.json"), "w") as f:
            json.dump(rows, f, indent=0)
    with open(fpath, "w") as f:
        json.dump(funnel, f, indent=1, sort_keys=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("cmd", choices=["candidates", "diffs", "check", "select"])
    ap.add_argument("--work", required=True)
    ap.add_argument("--raw")
    ap.add_argument("--out")
    ap.add_argument("--jobs", type=int, default=4)
    ap.add_argument("--repos", help="comma-separated names from repos.json (default: all)")
    a = ap.parse_args()
    a.work = os.path.abspath(a.work)
    a.repos = set(a.repos.split(",")) if a.repos else set()
    unknown = a.repos - {r["name"] for r in CFG["repos"]}
    if unknown:
        raise SystemExit(f"unknown repositories: {sorted(unknown)}")
    {"candidates": cmd_candidates, "diffs": cmd_diffs, "check": cmd_check, "select": cmd_select}[a.cmd](a)


if __name__ == "__main__":
    main()
