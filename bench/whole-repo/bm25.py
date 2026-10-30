#!/usr/bin/env python3
"""File-level BM25: the retrieval baseline of the SWE-bench paper, over whole files, filled to a token budget.

The documents are the files truncation may read (tokens.truncation_order: source, test, docs and changelog
files; vendored, generated and binary files left out). A document is the file's path, a newline and its text.

Analyzer: words are maximal runs of [A-Za-z0-9_]. Each word, lowercased and with underscores trimmed at its
ends, is a term; when the word splits into more than one part at underscores and at camelCase and digit
boundaries, each part is a term too (getUserName gives getusername, get, user and name). Terms shorter than
two characters and the 33 English stop words of Lucene's default English analyzer are dropped; no stemming.

Scoring: Okapi BM25 with k1 = 0.9 and b = 0.4 (the Pyserini defaults that the paper's retrieval used) and
Lucene's idf, log(1 + (N - df + 0.5) / (df + 0.5)), summed over the query's terms with their multiplicity.
Files with a positive score are ranked by score, ties by path.

Filling a budget: in rank order, each file goes in whole, wrapped as truncation wraps it, if it still fits;
otherwise the next one is tried (the paper's rule, here over the whole ranking rather than its top 20). The
joined text is then counted again, and files are removed from the end while it passes the budget.

usage: bm25.py rank --work WORKDIR --name NAME --query-file FILE [--top 20]
"""
import argparse
import math
import os
import re
import sys
import threading
from collections import Counter
from functools import lru_cache

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from tokens import load_manifest, read_text, truncation_order, wrap  # noqa: E402

K1, B = 0.9, 0.4
STOP = frozenset("a an and are as at be but by for if in into is it no not of on or such that the their then "
                 "there these they this to was will with".split())
WORD = re.compile(r"[A-Za-z0-9_]+")
PART = re.compile(r"[A-Z]+(?![a-z])|[A-Z]?[a-z]+|[0-9]+")


@lru_cache(maxsize=1 << 20)
def terms_of_word(word):
    out = []
    w = word.strip("_").lower()
    if len(w) >= 2 and w not in STOP:
        out.append(w)
    parts = PART.findall(word)
    if len(parts) > 1:
        out += [p.lower() for p in parts if len(p) >= 2 and p.lower() not in STOP]
    return tuple(out)


def analyze(text):
    """Counter of the text's terms."""
    c = Counter()
    for word, n in Counter(WORD.findall(text)).items():
        for t in terms_of_word(word):
            c[t] += n
    return c


def rank(query, docs):
    """docs: iterable of (path, text). Returns [(path, score)] for the files with a positive score."""
    q = analyze(query)
    qset = set(q)
    stats, total = [], 0
    for path, text in docs:
        tc = analyze(path + "\n" + text)
        dl = sum(tc.values())
        total += dl
        stats.append((path, dl, {t: tc[t] for t in qset if t in tc}))
    n = len(stats)
    if not n or not q:
        return []
    avgdl = total / n
    df = Counter(t for _p, _dl, tf in stats for t in tf)
    idf = {t: math.log(1 + (n - df[t] + 0.5) / (df[t] + 0.5)) for t in qset}
    scored = []
    for path, dl, tf in stats:
        if not tf:
            continue
        norm = K1 * (1 - B + B * dl / avgdl) if avgdl else K1
        s = sum(q[t] * idf[t] * f * (K1 + 1) / (f + norm) for t, f in tf.items())
        if s > 0:
            scored.append((path, s))
    scored.sort(key=lambda x: (-x[1], x[0]))
    return scored


class Index:
    """The same scoring as rank() over an inverted index built once, for many questions on one snapshot (the
    whole-repository benchmark asks up to 40 per repository). Scores equal rank()'s up to the order in which
    floating-point terms are added (here: the query's terms in sorted order)."""

    def __init__(self, docs):
        self.paths, self.dl, self.post = [], [], {}
        for path, text in docs:
            i = len(self.paths)
            tc = analyze(path + "\n" + text)
            self.paths.append(path)
            self.dl.append(sum(tc.values()))
            for t, f in tc.items():
                self.post.setdefault(t, []).append((i, f))
        self.n = len(self.paths)
        self.avgdl = sum(self.dl) / self.n if self.n else 0.0

    def rank(self, query):
        q = analyze(query)
        scores = {}
        for t in sorted(q):
            plist = self.post.get(t)
            if not plist:
                continue
            df = len(plist)
            w = q[t] * math.log(1 + (self.n - df + 0.5) / (df + 0.5))
            for i, f in plist:
                norm = K1 * (1 - B + B * self.dl[i] / self.avgdl) if self.avgdl else K1
                scores[i] = scores.get(i, 0.0) + w * f * (K1 + 1) / (f + norm)
        out = [(self.paths[i], s) for i, s in scores.items() if s > 0]
        out.sort(key=lambda x: (-x[1], x[0]))
        return out


_INDEXES, _LOCK = {}, threading.Lock()


def eligible(work, name):
    """[(path, tokens of the text plus its wrapper tags)] in truncation's file set, from the manifest."""
    rows, counter_name = load_manifest(work, name)
    return [(p, c + w) for _r, p, c, w in truncation_order(name, rows)], counter_name


def ranking(work, name, query, reuse=False):
    """(ranked files, {path: tokens}, the manifest's counter). reuse=True keeps an Index of the snapshot in
    memory for later questions on it (one per process, built under a lock)."""
    files, counter_name = eligible(work, name)
    src = os.path.join(work, "src", name)

    def docs():
        for p, _n in files:
            t = read_text(os.path.join(src, p))
            if t is not None:
                yield p, t
    if not reuse:
        return rank(query, docs()), dict(files), counter_name
    key = (os.path.abspath(work), name)
    with _LOCK:
        if key not in _INDEXES:
            _INDEXES[key] = Index(docs())
    return _INDEXES[key].rank(query), dict(files), counter_name


def fill(ranked, sizes, budget, src, counter, verify=True):
    """(text, included paths, tokens): whole files in rank order, each if it still fits."""
    used, parts, included = 0, [], []
    for p, _s in ranked:
        n = sizes.get(p)
        if n is None or used + n > budget:
            continue
        t = read_text(os.path.join(src, p))
        if t is None:
            continue
        parts.append(wrap(p, t))
        included.append(p)
        used += n
    text = "".join(parts)
    total = counter.count(text) if verify else used
    while total > budget and parts:  # joins can tokenize differently from the parts; trim from the end
        parts.pop()
        included.pop()
        text = "".join(parts)
        total = counter.count(text)
    return text, included, total


def build_bm25(work, name, query, budget, counter, cache=None, reuse=False):
    """The BM25 context of at most `budget` tokens for one question; `cache` (a dict) keeps the ranking
    of (name, query) across budgets; reuse=True ranks with an Index kept for the snapshot's later questions."""
    key = (name, query)
    if cache is not None and key in cache:
        ranked, sizes, counter_name = cache[key]
    else:
        ranked, sizes, counter_name = ranking(work, name, query, reuse=reuse)
        if cache is not None:
            cache[key] = (ranked, sizes, counter_name)
    if counter_name != counter.name:
        raise SystemExit(f"{name}: the manifest was counted with {counter_name}, not {counter.name}")
    return fill(ranked, sizes, budget, os.path.join(work, "src", name), counter)


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("rank")
    r.add_argument("--work", required=True)
    r.add_argument("--name", required=True)
    r.add_argument("--query-file", required=True)
    r.add_argument("--top", type=int, default=20)
    a = ap.parse_args()
    ranked, _sizes, _c = ranking(a.work, a.name, open(a.query_file, encoding="utf-8").read())
    for p, s in ranked[:a.top]:
        print(f"{s:.3f}\t{p}")


if __name__ == "__main__":
    main()
