#!/usr/bin/env python3
"""Bug localization without an LLM: does the tool's answer to the issue text
contain the files the real fix changed?

For SWE-bench Lite instances (princeton-nlp/SWE-bench_Lite), the repository is
checked out at the instance's base commit, the graph is built with
`graphicle-graph extract --code-only`, and each method answers the issue text
(`problem_statement`) within the same token budget:

  graphify   `query "<issue>" --budget B`: Graphify's query answer (native and
             Python outputs are identical; checked by the parity suite)
  retrieve   `retrieve "<issue>" --budget B --json` (opt-in, this port)
  bm25       BM25 over whole files: the plain sparse baseline of SWE-bench

Scores: hit@k (a changed file in the first k files), recall@k (share of the
changed files in the first k) and the answer's size in tokens (chars / 4).

Usage:
  localize.py --binary BIN --dataset lite.json --work DIR --out DIR
      [--repos django/django,psf/requests,...] [--limit-per-repo N] [--skip-per-repo N]
      [--budget 2000] [--jobs N] [--features]

--features also saves retrieve's per-file score parts (`retrieve --json --explain`)
and the BM25 ranking to features.jsonl, for tuning the ranking offline.
"""
import argparse, json, math, os, re, subprocess, sys, time
from collections import Counter, defaultdict
from pathlib import Path

WORD = re.compile(r"[A-Za-z_][A-Za-z0-9_]+")
KS = (1, 3, 5, 10)


def gold_files(patch):
    return sorted({m.group(1) for m in re.finditer(r"^diff --git a/(\S+) b/", patch, re.M)})


def git(repo, *args, check=True):
    return subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=True, check=check)


def checkout(work, repo_name, commit):
    repo = work / repo_name.replace("/", "__")
    if not repo.exists():
        subprocess.run(["git", "clone", "-q", "--filter=blob:none", "--no-checkout",
                        f"https://github.com/{repo_name}", str(repo)], check=True)
    git(repo, "checkout", "-q", "--force", commit)
    git(repo, "clean", "-fdqx")
    return repo


def tokens(text):
    out = []
    for w in WORD.findall(text):
        lw = w.lower()
        out.append(lw)
        parts = [p.lower() for p in re.split(r"_|(?<=[a-z0-9])(?=[A-Z])", w) if len(p) > 1]
        if len(parts) > 1:
            out.extend(parts)
    return out


def bm25_files(repo, question, files):
    docs = []
    for rel in files:
        try:
            text = (repo / rel).read_text(errors="replace")
        except OSError:
            continue
        docs.append((rel, Counter(tokens(text + " " + rel.replace("/", " ")))))
    q = Counter(tokens(question))
    df = Counter(t for _, tf in docs for t in tf if t in q)
    n = len(docs) or 1
    avg = sum(sum(tf.values()) for _, tf in docs) / n
    scores = []
    for rel, tf in docs:
        length = sum(tf.values())
        s = 0.0
        for t in q:
            f = tf.get(t, 0)
            if f:
                idf = math.log((n - df[t] + 0.5) / (df[t] + 0.5) + 1)
                s += idf * f * 2.2 / (f + 1.2 * (0.25 + 0.75 * length / avg))
        scores.append((s, rel))
    scores.sort(key=lambda x: (-x[0], x[1]))
    return [rel for s, rel in scores if s > 0]


def ordered_unique(items):
    seen, out = set(), []
    for x in items:
        if x and x not in seen:
            seen.add(x)
            out.append(x)
    return out


def run(binary, args, env):
    t0 = time.perf_counter()
    res = subprocess.run([binary, *args], capture_output=True, text=True, env=env, timeout=1800)
    return res, time.perf_counter() - t0


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--binary", required=True)
    ap.add_argument("--dataset", required=True, type=Path)
    ap.add_argument("--work", required=True, type=Path)
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument("--repos", default="django/django,psf/requests,pallets/flask,pytest-dev/pytest")
    ap.add_argument("--limit-per-repo", type=int, default=0)
    ap.add_argument("--skip-per-repo", type=int, default=0,
                    help="skip each repository's first N instances (a held-out slice after --limit-per-repo runs)")
    ap.add_argument("--budget", type=int, default=2000)
    ap.add_argument("--jobs", type=int, default=1, help="repositories processed in parallel")
    ap.add_argument("--features", action="store_true",
                    help="also save retrieve's per-file score parts (retrieve --explain) to features.jsonl")
    args = ap.parse_args()
    binary = os.path.abspath(args.binary)
    args.work.mkdir(parents=True, exist_ok=True)
    args.out.mkdir(parents=True, exist_ok=True)
    wanted = args.repos.split(",")
    rows = [r for r in json.loads(args.dataset.read_text()) if r["repo"] in wanted]
    per = defaultdict(int)
    kept = []
    for r in rows:
        per[r["repo"]] += 1
        if per[r["repo"]] <= args.skip_per_repo:
            continue
        if args.limit_per_repo and per[r["repo"]] > args.skip_per_repo + args.limit_per_repo:
            continue
        kept.append(r)
    rows = kept
    env = {k: v for k, v in os.environ.items() if not re.search(r"API_KEY|_TOKEN|SECRET", k)}
    env["GRAPHIFY_NO_TIPS"] = "1"
    groups = defaultdict(list)
    for r in rows:
        groups[r["repo"]].append(r)
    done = [0]
    features = open(args.out / "features.jsonl", "w") if args.features else None
    import threading
    lock = threading.Lock()

    def one(row):
        gold = gold_files(row["patch"])
        repo = checkout(args.work, row["repo"], row["base_commit"])
        out = args.work / "out" / row["instance_id"]
        env_i = dict(env, GRAPHIFY_OUT=str(out))
        res, build_s = run(binary, ["extract", str(repo), "--code-only", "--force"], env_i)
        graph = out / "graph.json"
        if res.returncode != 0 or not graph.exists():
            print(f"{row['instance_id']}: extract failed: {res.stderr[-300:]}", file=sys.stderr)
            return None
        question = row["problem_statement"]
        entry = {"instance_id": row["instance_id"], "repo": row["repo"], "gold": gold,
                 "build_s": round(build_s, 2), "methods": {}}
        res, secs = run(binary, ["query", question, "--graph", str(graph), "--budget", str(args.budget)], env_i)
        files = ordered_unique(m.group(1) for m in re.finditer(r"\[src=([^\s\]]+)", res.stdout))
        entry["methods"]["graphify"] = {"files": files[:20], "tokens": len(res.stdout) // 4, "seconds": round(secs, 3)}
        argv = ["retrieve", question, "--graph", str(graph), "--budget", str(args.budget), "--root", str(repo), "--json"]
        res, secs = run(binary, argv + (["--explain"] if features else []), env_i)
        try:
            answer = json.loads(res.stdout.strip().splitlines()[-1]) if res.stdout.strip() else {}
        except (ValueError, IndexError):
            answer = {}
        files = ordered_unique(i["file"] for i in answer.get("items", []))
        res_text, _ = run(binary, ["retrieve", question, "--graph", str(graph), "--budget", str(args.budget),
                                   "--root", str(repo)], env_i)
        entry["methods"]["retrieve"] = {"files": files[:20], "tokens": len(res_text.stdout) // 4, "seconds": round(secs, 3)}
        tracked = [f for f in git(repo, "ls-files").stdout.splitlines() if f.endswith(".py")]
        t0 = time.perf_counter()
        files = bm25_files(repo, question, tracked)
        entry["methods"]["bm25"] = {"files": files[:20], "tokens": None, "seconds": round(time.perf_counter() - t0, 3)}
        if features is not None:
            with lock:
                features.write(json.dumps({"instance_id": row["instance_id"], "repo": row["repo"], "gold": gold,
                                           "ranking": answer.get("ranking", []), "bm25": files[:100]}) + "\n")
                features.flush()
        subprocess.run(["rm", "-rf", str(out)])
        with lock:
            done[0] += 1
            n = done[0]
        hits = {m: any(g in v["files"][:5] for g in gold) for m, v in entry["methods"].items()}
        print(f"[{n}/{len(rows)}] {row['instance_id']} build {build_s:.1f}s hit@5 {hits}", file=sys.stderr)
        return entry

    def group(items):
        return [e for e in (one(r) for r in items) if e is not None]

    from concurrent.futures import ThreadPoolExecutor
    with ThreadPoolExecutor(max_workers=max(1, args.jobs)) as pool:
        by_repo = dict(zip(groups, pool.map(group, groups.values())))
    order = {r["instance_id"]: n for n, r in enumerate(rows)}
    results = sorted((e for es in by_repo.values() for e in es), key=lambda e: order[e["instance_id"]])
    if features is not None:
        features.close()
    import hashlib
    summary = {"instances": len(results), "budget": args.budget, "repos": wanted,
               "binary_sha256": hashlib.sha256(Path(binary).read_bytes()).hexdigest(), "methods": {}}
    for m in ("graphify", "retrieve", "bm25"):
        agg = {}
        for k in KS:
            agg[f"hit@{k}"] = round(sum(any(g in r["methods"][m]["files"][:k] for g in r["gold"]) for r in results) / max(len(results), 1), 4)
            agg[f"recall@{k}"] = round(sum(len(set(r["gold"]) & set(r["methods"][m]["files"][:k])) / len(r["gold"]) for r in results) / max(len(results), 1), 4)
        toks = [r["methods"][m]["tokens"] for r in results if r["methods"][m]["tokens"] is not None]
        agg["median_tokens"] = sorted(toks)[len(toks) // 2] if toks else None
        secs = sorted(r["methods"][m]["seconds"] for r in results)
        agg["median_seconds"] = secs[len(secs) // 2] if secs else None
        summary["methods"][m] = agg
    (args.out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    (args.out / "instances.json").write_text(json.dumps(results, indent=1) + "\n")
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    sys.exit(main())
