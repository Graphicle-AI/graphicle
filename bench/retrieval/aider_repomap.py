#!/usr/bin/env python3
"""Aider's repository map as a localization baseline, paired with localize.py.

Portions copied from Aider (https://github.com/Aider-AI/aider, aider-chat
0.86.2, aider/coders/base_coder.py): Coder.get_file_mentions,
get_ident_mentions and get_ident_filename_matches, as file_mentions,
ident_mentions and ident_filename_matches below. Aider is licensed under the
Apache License, Version 2.0, the same text as this repository's LICENSE. The
copies are modified (no files in the chat, every tracked file addable); see
bench/THIRD_PARTY.md.

Aider (aider-chat) gives its model a "repo map": the tree-sitter definitions of
the repository's files, ranked with personalized PageRank over the def/ref
graph and fitted to a token budget. With no files in the chat, the
personalization comes from the files and identifiers the user's message
mentions. This harness builds that map for each SWE-bench Lite issue with the
upstream RepoMap code, on the same checkouts, dataset rows and token budget as
localize.py:

  aider       the files in the budget-fitted map, in PageRank order: what the
              model would see, at the budget retrieve answers in
  aider_rank  Aider's whole PageRank file ranking, ignoring the budget (an upper
              bound for the ranking itself)
  aider_chat  the files the issue names, sorted as Aider offers them for the
              chat, then the map Aider rebuilds with them in the chat. A
              mentioned file only seeds the PageRank walk, so the map ranks
              what it references, not the file itself; Aider instead offers to
              add the files a model reply names ("Add file to the chat?",
              Coder.check_for_file_mentions). This assumes the reply names
              every file the issue does and every offer is accepted: a
              generous reading of the baseline.

The issue text is the chat message. Its mentions come from copies of
Coder.get_file_mentions, get_ident_mentions and get_ident_filename_matches
(aider/coders/base_coder.py), with every tracked file addable, as in
Coder.get_repo_map, including its fallbacks to the whole-repository and then the
unhinted map. RepoMap.get_repo_map's 8x larger map for an empty chat is not
applied, so both tools answer in the same budget. Tokens are chars / 4, the
estimate localize.py reports. Tags caches live under --work, outside the
checkouts.

One change to upstream makes the map reproducible. py-tree-sitter 0.25 returns
each query capture's nodes in an order that depends on memory addresses, and
RepoMap.get_tags_raw repeats each capture's last node. As a result, upstream
reference weights, and with them the order of near-tied files, differ from run
to run. The harness sorts each capture's nodes into document order before
RepoMap reads them. Their tags are cached under aider-tags-ordered, so tags
cached without the sort are not reused.

Usage (upstream aider on Python 3.12; PYTHONHASHSEED is pinned to 0 because
RepoMap orders equal-rank tags through sets; on macOS, aider-chat 0.86.2's
pinned SciPy wheel fails to load, so install it with an override of
scipy>=1.16):
  uv run --no-project --python 3.12 --with aider-chat==0.86.2 \\
    python bench/retrieval/aider_repomap.py --dataset lite.json --work DIR --out DIR \\
    [--repos ...] [--limit-per-repo N] [--skip-per-repo N] [--budget 2000] [--jobs N] \\
    [--pair localize-instances.json]

--pair compares per-instance hit@k with a localize.py run on the same
instances (exact McNemar test on the discordant pairs).
"""
import argparse, hashlib, json, math, os, platform, re, sys, time
from collections import defaultdict
from concurrent.futures import ProcessPoolExecutor
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from localize import KS, checkout, git, gold_files, ordered_unique  # noqa: E402

METHODS = ("aider", "aider_rank", "aider_chat")


class Model:
    def token_count(self, text):
        return len(text) / 4


class IO:
    def tool_output(self, *args, **kwargs):
        pass

    tool_warning = tool_error = tool_output

    def read_text(self, filename, silent=False):
        try:
            with open(str(filename), "r", encoding="utf-8") as f:
                return f.read()
        except (OSError, UnicodeError):
            return None


def file_mentions(content, rel_fnames):
    """Coder.get_file_mentions with no files in the chat."""
    words = set(word for word in content.split())
    words = set(word.rstrip(",.!;:?") for word in words)
    quotes = "\"'`*_"
    words = set(word.strip(quotes) for word in words)
    normalized_words = set(word.replace("\\", "/") for word in words)
    mentioned, by_name = set(), {}
    for rel in rel_fnames:
        if rel.replace("\\", "/") in normalized_words:
            mentioned.add(rel)
        name = os.path.basename(rel)
        if "/" in name or "\\" in name or "." in name or "_" in name or "-" in name:
            by_name.setdefault(name, []).append(rel)
    for name, rels in by_name.items():
        if len(rels) == 1 and name in words:
            mentioned.add(rels[0])
    return mentioned


def ident_mentions(text):
    """Coder.get_ident_mentions."""
    return set(re.split(r"\W+", text))


def ident_filename_matches(idents, rel_fnames):
    """Coder.get_ident_filename_matches."""
    stems = defaultdict(set)
    for rel in rel_fnames:
        if not rel or rel == ".":
            continue
        base = Path(rel).stem.lower()
        if len(base) >= 5:
            stems[base].add(rel)
    matches = set()
    for ident in idents:
        if len(ident) >= 5:
            matches.update(stems.get(ident.lower(), ()))
    return matches


def map_files(tree, rel_set):
    """Files RepoMap.to_tree lists: a header line `path:` (with definitions) or `path`."""
    out = []
    for line in (tree or "").splitlines():
        name = line[:-1] if line.endswith(":") else line
        if name in rel_set:
            out.append(name)
    return ordered_unique(out)


def one(repo_map, work, row, budget):
    gold = gold_files(row["patch"])
    repo = checkout(work, row["repo"], row["base_commit"])
    rel = sorted({f for f in git(repo, "ls-files", "-z").stdout.split("\0") if f})
    text = row["problem_statement"]
    idents = ident_mentions(text)
    fnames = file_mentions(text, rel) | ident_filename_matches(idents, rel)
    rm = repo_map(map_tokens=budget, root=str(repo), main_model=Model(), io=IO())
    captured = {}
    upstream = rm.get_ranked_tags

    def capture(*args, **kwargs):
        captured["tags"] = upstream(*args, **kwargs)
        return captured["tags"]

    def fitted(chat, other, *hints):
        tree = rm.get_ranked_tags_map_uncached([str(repo / f) for f in chat], [str(repo / f) for f in other],
                                               budget, *hints)
        rank = ordered_unique(t[0] for t in captured.get("tags", [])) if tree else []
        shown = set(map_files(tree, set(rel)))
        return tree, rank, [f for f in rank if f in shown]

    rm.get_ranked_tags = capture
    t0 = time.perf_counter()
    tree, rank, in_map = fitted([], rel, fnames, idents)
    hinted = bool(tree)
    if not tree:
        tree, rank, in_map = fitted([], rel)
    secs = time.perf_counter() - t0
    chat = sorted(file_mentions(text, rel))
    follow, follow_secs = in_map, 0.0
    if chat:
        t1 = time.perf_counter()
        # Coder.get_repo_map with the accepted files in the chat; an empty map falls back to the one above.
        chat_tree, _, chat_map = fitted(chat, [f for f in rel if f not in set(chat)], fnames, idents)
        follow_secs = time.perf_counter() - t1
        if chat_tree:
            follow = chat_map
    shown = set(map_files(tree, set(rel)))
    return {"instance_id": row["instance_id"], "repo": row["repo"], "gold": gold,
            "mentioned_gold": sorted(set(gold) & fnames), "mentioned_files": len(fnames), "hinted": hinted,
            "chat_files": len(chat), "chat_gold": sorted(set(gold) & set(chat)),
            "map_files": len(shown), "map_is_rank_prefix": in_map == rank[:len(in_map)],
            "methods": {"aider": {"files": in_map[:20], "tokens": len(tree or "") // 4, "seconds": round(secs, 3)},
                        "aider_rank": {"files": rank[:20], "tokens": None, "seconds": round(secs, 3)},
                        "aider_chat": {"files": ordered_unique(chat + follow)[:20], "tokens": None,
                                       "seconds": round(secs + follow_secs, 3)}}}


def repo_map_class(work, repo_name):
    from aider.repomap import RepoMap

    def run_captures(self, query, node):
        return {name: sorted(nodes, key=lambda n: (n.start_byte, n.end_byte))
                for name, nodes in RepoMap._run_captures(self, query, node).items()}

    cache = Path(work) / "aider-tags-ordered" / repo_name.replace("/", "__")
    return type("RepoMapAt", (RepoMap,), {"TAGS_CACHE_DIR": str(cache), "_run_captures": run_captures})


def group(job):
    work, rows, budget = job
    work = Path(work)
    repo_map = repo_map_class(work, rows[0]["repo"])
    out = []
    for row in rows:
        entry = one(repo_map, work, row, budget)
        hits = {m: any(g in v["files"][:5] for g in entry["gold"]) for m, v in entry["methods"].items()}
        print(f"{row['instance_id']} map {entry['map_files']} files {entry['methods']['aider']['seconds']:.1f}s"
              f" hit@5 {hits}", file=sys.stderr, flush=True)
        out.append(entry)
    return out


def mcnemar(b, c):
    n = b + c
    if n == 0:
        return 1.0
    return min(1.0, 2 * sum(math.comb(n, i) for i in range(min(b, c) + 1)) / 2 ** n)


def hit(entry, files, k):
    return any(g in files[:k] for g in entry["gold"])


def paired(results, path):
    stored = {e["instance_id"]: e for e in json.loads(Path(path).read_text())}
    common = [r for r in results if r["instance_id"] in stored]
    if not common:
        return {"instances": 0}
    others = list(stored[common[0]["instance_id"]]["methods"])
    comparisons = {}
    for mine in METHODS:
        for other in others:
            cells = {}
            for k in KS:
                a = [hit(r, r["methods"][mine]["files"], k) for r in common]
                b = [hit(r, stored[r["instance_id"]]["methods"][other]["files"], k) for r in common]
                only_a = sum(x and not y for x, y in zip(a, b))
                only_b = sum(y and not x for x, y in zip(a, b))
                cells[f"hit@{k}"] = {mine: round(sum(a) / len(common), 4), other: round(sum(b) / len(common), 4),
                                     f"{mine}_only": only_a, f"{other}_only": only_b,
                                     "mcnemar_p": round(mcnemar(only_a, only_b), 4)}
            comparisons[f"{mine} vs {other}"] = cells
    return {"instances": len(common), "of_stored": len(stored),
            "gold_identical": all(stored[r["instance_id"]]["gold"] == r["gold"] for r in common),
            "comparisons": comparisons}


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--dataset", required=True, type=Path)
    ap.add_argument("--work", required=True, type=Path)
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument("--repos", default="django/django,psf/requests,pallets/flask,pytest-dev/pytest")
    ap.add_argument("--limit-per-repo", type=int, default=0)
    ap.add_argument("--skip-per-repo", type=int, default=0)
    ap.add_argument("--budget", type=int, default=2000)
    ap.add_argument("--jobs", type=int, default=1, help="repositories processed in parallel (one process each)")
    ap.add_argument("--pair", type=Path, help="a localize.py instances.json on the same instances")
    args = ap.parse_args()
    if os.environ.get("PYTHONHASHSEED") != "0":
        os.execve(sys.executable, [sys.executable, *sys.argv], dict(os.environ, PYTHONHASHSEED="0"))
    import importlib.metadata as metadata
    args.work = args.work.resolve()
    args.work.mkdir(parents=True, exist_ok=True)
    args.out.mkdir(parents=True, exist_ok=True)
    wanted = args.repos.split(",")
    per, rows = defaultdict(int), []
    for r in json.loads(args.dataset.read_text()):
        if r["repo"] not in wanted:
            continue
        per[r["repo"]] += 1
        if per[r["repo"]] <= args.skip_per_repo:
            continue
        if args.limit_per_repo and per[r["repo"]] > args.skip_per_repo + args.limit_per_repo:
            continue
        rows.append(r)
    groups = defaultdict(list)
    for r in rows:
        groups[r["repo"]].append(r)
    jobs = sorted(groups.values(), key=len, reverse=True)
    with ProcessPoolExecutor(max_workers=max(1, args.jobs)) as pool:
        done = list(pool.map(group, [(str(args.work), g, args.budget) for g in jobs]))
    order = {r["instance_id"]: n for n, r in enumerate(rows)}
    results = sorted((e for es in done for e in es), key=lambda e: order[e["instance_id"]])
    n = max(len(results), 1)
    summary = {"instances": len(results), "budget": args.budget, "repos": wanted,
               "aider_chat": metadata.version("aider-chat"),
               "packages": {p: metadata.version(p) for p in ("grep-ast", "tree-sitter", "tree-sitter-language-pack",
                                                             "networkx", "scipy")},
               "capture_order": "document",
               "python": platform.python_version(),
               "dataset_sha256": hashlib.sha256(args.dataset.read_bytes()).hexdigest(),
               "mentioned_gold": round(sum(bool(r["mentioned_gold"]) for r in results) / n, 4),
               "chat_gold": round(sum(bool(r["chat_gold"]) for r in results) / n, 4),
               "unhinted_fallbacks": sum(not r["hinted"] for r in results),
               "map_is_rank_prefix": all(r["map_is_rank_prefix"] for r in results),
               "median_map_files": sorted(r["map_files"] for r in results)[len(results) // 2] if results else None,
               "methods": {}}
    for m in METHODS:
        agg = {}
        for k in KS:
            agg[f"hit@{k}"] = round(sum(hit(r, r["methods"][m]["files"], k) for r in results) / n, 4)
            agg[f"recall@{k}"] = round(sum(len(set(r["gold"]) & set(r["methods"][m]["files"][:k])) / len(r["gold"])
                                           for r in results) / n, 4)
        toks = sorted(r["methods"][m]["tokens"] for r in results if r["methods"][m]["tokens"] is not None)
        agg["median_tokens"] = toks[len(toks) // 2] if toks else None
        secs = sorted(r["methods"][m]["seconds"] for r in results)
        agg["median_seconds"] = secs[len(secs) // 2] if secs else None
        summary["methods"][m] = agg
    if args.pair:
        summary["paired"] = paired(results, args.pair)
    (args.out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    (args.out / "instances.json").write_text(json.dumps(results, indent=1) + "\n")
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    sys.exit(main())
