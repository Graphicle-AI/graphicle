#!/usr/bin/env python3
"""Token counts for the whole-repository benchmark: one counter for every arm with a budget.

usage:
  tokens.py sizes --work WORKDIR [--tokenizer FILE] [--out FILE] [NAME ...]
      counts every tracked file of each checkout at S by class (paths.py) and writes
      WORKDIR/manifest/NAME.tsv (path, class, bytes, tokens of the text, tokens of truncation's
      wrapper tags; binary files get tokens -1)
      and a summary of the sizes to --out
  tokens.py count [--tokenizer FILE] < TEXT
  tokens.py check-pack [--tokenizer FILE] PACK.json ...
      compares the token counts in `graphicle pack --format json` output with this counter (exit 1 when
      they differ by more than 2%): truncation and the pack must measure their budgets alike

The counter: with --tokenizer (a tokenizer.json in the Hugging Face format, read with the
`tokenizers` package), exact tokens. Without it, the estimate: ceil(UTF-8 bytes / 4). Every result
records which one was used (counter_name). The pack arm gets the same tokenizer.json through
`graphicle pack --tokenizer`, so its budget and truncation's are measured in the same units.
"""
import argparse
import json
import math
import os
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from paths import classify  # noqa: E402


class Counter:
    def __init__(self, tokenizer=None):
        self.tok = None
        if tokenizer:
            from tokenizers import Tokenizer
            self.tok = Tokenizer.from_file(tokenizer)
            self.name = "exact:" + os.path.basename(os.path.dirname(os.path.abspath(tokenizer))) + "/" + os.path.basename(tokenizer)
        else:
            self.name = "estimate:utf8-bytes/4"

    def count(self, text):
        if self.tok is None:
            return math.ceil(len(text.encode("utf-8")) / 4)
        return len(self.tok.encode(text, add_special_tokens=False).ids)

    def count_many(self, texts):
        if self.tok is None:
            return [self.count(t) for t in texts]
        return [len(e.ids) for e in self.tok.encode_batch(texts, add_special_tokens=False)]


def tracked_files(src):
    out = subprocess.run(["git", "ls-files", "-z"], cwd=src, capture_output=True, text=True, check=True).stdout
    return sorted(p for p in out.split("\0") if p)


def read_text(path):
    """The file's text, or None for a binary file, a symlink or anything that is not a regular file."""
    if os.path.islink(path) or not os.path.isfile(path):
        return None
    with open(path, "rb") as f:
        data = f.read()
    if b"\0" in data[:8192]:
        return None
    return data.decode("utf-8", errors="replace")


def line_count(text):
    return text.count("\n") + (0 if text.endswith("\n") or not text else 1)


def wrap_parts(path, text):
    """How truncation wraps a file, as the pack wraps its chunks: (opening tag, body, closing tag)."""
    body = text if text.endswith("\n") or not text else text + "\n"
    return f'<file path="{path}" lines="1-{line_count(text)}">\n', body, "</file>\n"


def wrap(path, text):
    return "".join(wrap_parts(path, text))


def build_manifest(work, name, counter):
    src = os.path.join(work, "src", name)
    paths = tracked_files(src)
    allp = set(paths)
    rows, batch = [], []

    def flush():
        if not batch:
            return
        counts = counter.count_many([t for (_, _, t) in batch])
        tags = counter.count_many([wrap_parts(p, t)[0] + wrap_parts(p, t)[2] for (p, _, t) in batch])
        for (p, cls, t), c, w in zip(batch, counts, tags):
            rows.append((p, cls, len(t.encode("utf-8")), c, w))
        batch.clear()

    for p in paths:
        cls = classify(name, p, allp)
        t = read_text(os.path.join(src, p))
        if t is None:
            full = os.path.join(src, p)
            size = os.path.getsize(full) if os.path.isfile(full) and not os.path.islink(full) else 0
            rows.append((p, cls, size, -1, 0))
            continue
        batch.append((p, cls, t))
        if len(batch) >= 2000:
            flush()
    flush()
    rows.sort()
    d = os.path.join(work, "manifest")
    os.makedirs(d, exist_ok=True)
    with open(os.path.join(d, name + ".tsv"), "w") as f:
        f.write(f"# counter\t{counter.name}\n")
        for p, cls, b, c, w in rows:
            f.write(f"{p}\t{cls}\t{b}\t{c}\t{w}\n")
    return rows


def load_manifest(work, name):
    rows, counter = [], None
    with open(os.path.join(work, "manifest", name + ".tsv")) as f:
        for line in f:
            if line.startswith("# counter\t"):
                counter = line.rstrip("\n").split("\t")[1]
                continue
            p, cls, b, c, w = line.rstrip("\n").split("\t")
            rows.append((p, cls, int(b), int(c), int(w)))
    return rows, counter


def truncation_order(name, rows):
    """Truncation's fixed order: source files, then tests, then docs and changelogs, each by path;
    vendored, generated and binary files left out."""
    from paths import truncation_rank
    keyed = [(truncation_rank(cls), p, c, w) for p, cls, _b, c, w in rows
             if c >= 0 and truncation_rank(cls) is not None]
    keyed.sort(key=lambda k: (k[0], k[1]))
    return keyed


def build_truncation(work, name, budget, counter, verify=True):
    """The truncation context of at most `budget` tokens: whole files in truncation_order, each
    wrapped like a pack chunk; a file that would pass the budget is skipped and the next one tried.
    Returns (text, included paths, tokens)."""
    rows, mcounter = load_manifest(work, name)
    if mcounter != counter.name:
        raise SystemExit(f"{name}: the manifest was counted with {mcounter}, not {counter.name}; run tokens.py sizes again")
    src = os.path.join(work, "src", name)
    used, parts, included = 0, [], []
    for _rank, p, c, w in truncation_order(name, rows):
        if used + c + w > budget:
            continue
        t = read_text(os.path.join(src, p))
        if t is None:
            continue
        parts.append(wrap(p, t))
        included.append(p)
        used += c + w
    text = "".join(parts)
    total = counter.count(text) if verify else used
    while total > budget and parts:  # joins can tokenize differently from the parts; trim from the end
        parts.pop()
        included.pop()
        text = "".join(parts)
        total = counter.count(text)
    return text, included, total


def summarize(rows):
    by = {}
    for _p, cls, b, c, _w in rows:
        d = by.setdefault(cls, {"files": 0, "bytes": 0, "tokens": 0, "binary_files": 0})
        if c < 0:
            d["binary_files"] += 1
            continue
        d["files"] += 1
        d["bytes"] += b
        d["tokens"] += c
    tot = lambda classes: sum(by.get(k, {}).get("tokens", 0) for k in classes)
    return {
        "by_class": by,
        "tokens_all": tot(["source", "test", "docs", "changelog", "vendored", "generated"]),
        "tokens_without_vendored_generated": tot(["source", "test", "docs", "changelog"]),
        "tokens_nontest_source": tot(["source"]),
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("cmd", choices=["sizes", "count", "check-pack"])
    ap.add_argument("--work")
    ap.add_argument("--tokenizer")
    ap.add_argument("--out")
    ap.add_argument("names", nargs="*")
    a = ap.parse_intermixed_args()
    counter = Counter(a.tokenizer)
    if a.cmd == "count":
        print(counter.count(sys.stdin.read()))
        return
    if a.cmd == "check-pack":
        # Pack JSON files (graphicle.pack.v1) as arguments: do the pack's token counts agree with this counter?
        ours = theirs = 0
        for path in a.names:
            pj = json.load(open(path))
            for sec in pj.get("sections") or []:
                for ch in sec.get("chunks") or []:
                    ours += counter.count(ch.get("text") or "")
                    theirs += ch.get("tokens") or 0
            print(f"{os.path.basename(path)}: pack tokenizer {pj.get('tokenizer')}, pack total {pj.get('tokens')}")
        ratio = ours / theirs if theirs else float("nan")
        print(f"chunk tokens: pack {theirs:,}, {counter.name} {ours:,}, ratio {ratio:.4f}")
        sys.exit(0 if theirs and abs(ratio - 1) <= 0.02 else 1)
    work = os.path.abspath(a.work)
    names = a.names or sorted(os.listdir(os.path.join(work, "src")))
    with ThreadPoolExecutor(min(5, len(names))) as ex:
        res = dict(zip(names, ex.map(lambda n: summarize(build_manifest(work, n, counter)), names)))
    out = {"counter": counter.name, "repos": res}
    if a.out:
        with open(a.out, "w") as f:
            json.dump(out, f, indent=1, sort_keys=True)
    for n, s in res.items():
        print(f"{n}: all {s['tokens_all']:,}  without vendored/generated {s['tokens_without_vendored_generated']:,}"
              f"  non-test source {s['tokens_nontest_source']:,}  ({counter.name})", flush=True)


if __name__ == "__main__":
    main()
