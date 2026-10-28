#!/usr/bin/env python3
"""Rebuild the question texts from GitHub and check them against the published hashes.

usage: fetch_texts.py --questions DIR --out DIR [--raw DIR]

The question sets publish each issue's URL and the sha256 of the question text, not the text. This
script fetches each issue with its edit history and title changes (the GraphQL query of
fetch_issues.sh), rebuilds the text with issue_text.py and writes OUT/texts-<name>.jsonl. It prints
how many texts match their hash; a mismatch means the issue's history changed on GitHub (for
example a deleted revision).

Without --raw it calls GitHub's GraphQL API: through the GitHub CLI when `gh` is installed and
logged in, else with the token in GITHUB_TOKEN. With --raw it reads pages that fetch_issues.sh
saved instead.
"""
import argparse
import glob
import json
import os
import subprocess
import sys
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from issue_text import question_text, sha256  # noqa: E402

FIELDS = """__typename
  ... on Issue {
    number url createdAt closedAt title body lastEditedAt
    userContentEdits(first: 100) { totalCount nodes { createdAt editedAt deletedAt diff } }
    timelineItems(itemTypes: [RENAMED_TITLE_EVENT], first: 50) {
      nodes { ... on RenamedTitleEvent { createdAt previousTitle currentTitle } }
    }
  }"""


def graphql(query):
    try:
        r = subprocess.run(["gh", "api", "graphql", "-f", "query=" + query], capture_output=True, text=True)
        if r.returncode == 0:
            return json.loads(r.stdout)
    except FileNotFoundError:
        pass
    token = os.environ.get("GITHUB_TOKEN")
    if not token:
        raise SystemExit("needs the GitHub CLI (logged in) or GITHUB_TOKEN")
    req = urllib.request.Request("https://api.github.com/graphql", data=json.dumps({"query": query}).encode(),
                                 headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=120) as resp:
        return json.loads(resp.read())


def read_raw(raw, name):
    from mine import read_pages
    out = {}
    for path in sorted(glob.glob(os.path.join(raw, f"issues-{name}-*.json"))):
        for page in read_pages(path):
            for _k, v in ((page.get("data") or {}).get("repository") or {}).items():
                if v and v.get("__typename") == "Issue":
                    out[v["url"]] = v
    return out


def fetch(slug, numbers):
    owner, repo = slug.split("/")
    out = {}
    for i in range(0, len(numbers), 20):
        part = numbers[i:i + 20]
        q = f'query {{ repository(owner: "{owner}", name: "{repo}") {{' + "".join(
            f" i{n}: issueOrPullRequest(number: {n}) {{ {FIELDS} }}" for n in part) + " } }"
        data = graphql(q)
        for _k, v in ((data.get("data") or {}).get("repository") or {}).items():
            if v and v.get("__typename") == "Issue":
                out[v["url"]] = v
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--questions", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--raw")
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    total = ok = 0
    for path in sorted(glob.glob(os.path.join(a.questions, "questions-*.jsonl"))):
        name = os.path.basename(path)[len("questions-"):-len(".jsonl")]
        qs = [json.loads(l) for l in open(path) if l.strip()]
        if a.raw:
            issues = read_raw(a.raw, name)
        else:
            issues = fetch(qs[0]["repo"], sorted({int(q["issue_url"].rsplit("/", 1)[1]) for q in qs}))
        good = 0
        with open(os.path.join(a.out, f"texts-{name}.jsonl"), "w") as f:
            for q in qs:
                iss = issues.get(q["issue_url"])
                text, notes = question_text(iss) if iss else ("", {"missing": True})
                match = bool(iss) and sha256(text) == q["text_sha256"]
                good += match
                f.write(json.dumps({"id": q["id"], "text": text, "notes": notes, "hash_ok": match}, sort_keys=True) + "\n")
        total += len(qs)
        ok += good
        print(f"{name}: {good}/{len(qs)} texts match their hash")
    print(f"all: {ok}/{total}")
    sys.exit(0 if ok == total else 1)


if __name__ == "__main__":
    main()
