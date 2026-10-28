"""The pack arm's context: `graphicle pack`, or `graphicle context` as a labelled stand-in.

pack_context(...) returns a dict: text, included (paths), tokens, seconds, source and, for the pack,
its layout fields (see layout_info):
- `graphicle pack QUESTION --project SRC --data-dir DATA --budget B --format xml [--tokenizer T]
  [--layout standard|cache] [--prefix-share F]` for the text, and the same with --format json for the
  included files, the tokens and the layout (the pack's output contract, graphicle.pack.v1);
- with standin=True, `graphicle context QUESTION --project SRC --data-dir DATA --max-tokens B
  --max-chars 4B --json`, with B capped at 16,000 tokens and 64,000 characters (the most `context`
  takes): its excerpts wrapped like pack chunks. This is for testing the plumbing only: `context`
  is built for short evidence packets, not for filling a large budget, so its numbers are never
  results of the pack.
A file is included when the context holds some of its text; names listed without text do not count.

pack_json(...) runs only the JSON form and also returns the call's wall seconds and peak memory,
the pack's top omitted files, the files its manifest names as left out (for recall.py) and the
layout fields.

The layout fields (additive to graphicle.pack.v1; builds without layouts have none of them):
"layout" (standard or cache), "prefix_share" (cache only), "prefix_tokens", "prefix_sha256" (hex of
the prefix text's bytes) and, per section, "part" (prefix or suffix). The prefix text is the
concatenation of the prefix sections' chunk text, a byte-prefix of the whole text (every chunk's
text in order). layout_info checks both.
"""
import hashlib
import json
import os
import re
import subprocess
import tempfile
import threading
import time

QUESTION_MAX_CHARS = 100000  # a command-line argument's limit; the model's prompt keeps the whole text
STANDIN_MAX_TOKENS = 16000
LEFT_OUT = re.compile(r"^- (.+?): ~\d+ tokens, rank (\d+)$")
# The arms that are `graphicle pack`, with their layout options.
PACK_ARMS = {"pack": (None, None), "pack-cache": ("cache", 0.9), "pack-stable": ("cache", 1.0)}


def run_measured(cmd, timeout=1800):
    """(exit code, stdout, stderr, wall seconds, peak resident memory of the process in MiB)."""
    with tempfile.TemporaryFile() as out, tempfile.TemporaryFile() as err:
        t0 = time.time()
        p = subprocess.Popen(cmd, stdout=out, stderr=err)
        timer = threading.Timer(timeout, p.kill)
        timer.start()
        try:
            _pid, status, ru = os.wait4(p.pid, 0)
        finally:
            timer.cancel()
        p.returncode = os.waitstatus_to_exitcode(status)
        secs = time.time() - t0
        out.seek(0)
        err.seek(0)
        return p.returncode, out.read().decode("utf-8", "replace"), err.read().decode("utf-8", "replace"), \
            secs, ru.ru_maxrss / 1024


def standin_packet_to_context(packet):
    """`graphicle context --json` (0.1.8): evidence items hold the text, sources (same id) the path and lines."""
    sources = {s.get("id"): s for s in packet.get("sources") or []}
    parts, paths = [], []
    for ex in packet.get("evidence") or []:
        s = sources.get(ex.get("id")) or {}
        p = s.get("path") or ex.get("title") or ""
        t = ex.get("text") or ""
        if not p or not t:
            continue
        lines = f"{s['line']}-{s['end_line']}" if s.get("line") and s.get("end_line") else ""
        parts.append(f'<file path="{p}" lines="{lines}">\n{t.rstrip()}\n</file>\n')
        if p not in paths:
            paths.append(p)
    return "".join(parts), paths, packet.get("tokens")


def pack_command(graphicle, src, data, question, budget, tokenizer, fmt, layout=None, prefix_share=None):
    cmd = [graphicle, "pack", question[:QUESTION_MAX_CHARS], "--project", src, "--data-dir", data,
           "--budget", str(budget), "--format", fmt]
    if tokenizer:
        cmd += ["--tokenizer", tokenizer]
    if layout:
        cmd += ["--layout", layout]
    if prefix_share is not None:
        cmd += ["--prefix-share", f"{prefix_share:g}"]
    return cmd


def layout_info(pj):
    """The layout fields of a graphicle.pack.v1 JSON, with checks of the prefix contract:
    prefix_ok (the prefix sections come first and their chunk text hashes to prefix_sha256),
    prefix_text_tokens (the prefix sections' tokens) and suffix_tokens."""
    secs = pj.get("sections") or []
    parts = [s.get("part") for s in secs]
    info = {"layout": pj.get("layout"), "prefix_share": pj.get("prefix_share"),
            "prefix_tokens": pj.get("prefix_tokens"), "prefix_sha256": pj.get("prefix_sha256"),
            "parts": {s.get("name"): s.get("part") for s in secs}}
    if not pj.get("prefix_sha256") or not any(parts):
        info["prefix_ok"] = None
        return info
    n_prefix = sum(1 for p in parts if p == "prefix")
    first = all(p == "prefix" for p in parts[:n_prefix]) and all(p == "suffix" for p in parts[n_prefix:])
    text = "".join(c.get("text") or "" for s in secs[:n_prefix] for c in s.get("chunks") or [])
    info["prefix_ok"] = bool(first and hashlib.sha256(text.encode("utf-8")).hexdigest() == pj["prefix_sha256"])
    info["prefix_text_tokens"] = sum(s.get("tokens") or 0 for s in secs[:n_prefix])
    info["suffix_tokens"] = (pj.get("tokens") or 0) - (pj.get("prefix_tokens") or 0)
    return info


def pack_json(graphicle, src, data, question, budget, tokenizer=None, layout=None, prefix_share=None):
    """The JSON pack plus measurements: dict with included {path: rank}, omitted [(path, rank)],
    left_out_listed {path: rank} (the manifest's list), tokens, tokenizer, seconds, peak_mib, sections
    and the layout fields (layout_info)."""
    code, out, err, secs, peak = run_measured(
        pack_command(graphicle, src, data, question, budget, tokenizer, "json", layout, prefix_share))
    if code != 0:
        raise RuntimeError("graphicle pack: " + err.strip()[:300])
    pj = json.loads(out)
    listed = {}
    for sec in pj.get("sections") or []:
        if sec.get("name") != "manifest":
            continue
        for ch in sec.get("chunks") or []:
            for line in (ch.get("text") or "").splitlines():
                m = LEFT_OUT.match(line.strip())
                if m:
                    listed[m.group(1)] = int(m.group(2))
    return {"included": {f["path"]: f.get("rank") for f in pj.get("included") or []},
            "omitted": [(f["path"], f.get("rank")) for f in pj.get("omitted") or []],
            "left_out_listed": listed, "tokens": pj.get("tokens"), "tokenizer": pj.get("tokenizer"),
            "seconds": round(secs, 3), "peak_mib": round(peak, 1), "pack_seconds": pj.get("seconds"),
            "sections": {s.get("name"): s.get("tokens") for s in pj.get("sections") or []},
            **layout_info(pj)}


def pack_context(graphicle, src, data, question, budget, tokenizer=None, standin=False, want_text=True,
                 layout=None, prefix_share=None):
    q = question[:QUESTION_MAX_CHARS]
    t0 = time.time()
    if standin:
        # graphicle context (0.1.8) takes at most 16,000 tokens and 64,000 characters, whatever the budget.
        cmd = [graphicle, "context", q, "--project", src, "--data-dir", data,
               "--max-tokens", str(min(budget, STANDIN_MAX_TOKENS)), "--max-chars", str(min(4 * budget, 64000)),
               "--more-files", "0", "--json"]
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=900)
        if r.returncode != 0:
            raise RuntimeError("graphicle context: " + r.stderr.strip()[:300])
        text, paths, ntok = standin_packet_to_context(json.loads(r.stdout))
        return {"text": text, "included": paths, "tokens": ntok, "seconds": time.time() - t0,
                "source": "graphicle context (stand-in, cl100k tokens)", "prefix_sha256": None}
    rj = subprocess.run(pack_command(graphicle, src, data, q, budget, tokenizer, "json", layout, prefix_share),
                        capture_output=True, text=True, timeout=1800)
    if rj.returncode != 0:
        raise RuntimeError("graphicle pack: " + rj.stderr.strip()[:300])
    pj = json.loads(rj.stdout)
    text = None
    if want_text:
        rx = subprocess.run(pack_command(graphicle, src, data, q, budget, tokenizer, "xml", layout, prefix_share),
                            capture_output=True, text=True, timeout=1800)
        if rx.returncode != 0:
            raise RuntimeError("graphicle pack: " + rx.stderr.strip()[:300])
        text = rx.stdout
    return {"text": text, "included": [f["path"] for f in pj.get("included") or []], "tokens": pj.get("tokens"),
            "seconds": time.time() - t0, "source": f"graphicle pack ({pj.get('tokenizer', '?')})", **layout_info(pj)}
