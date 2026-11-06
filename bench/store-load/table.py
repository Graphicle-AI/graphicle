#!/usr/bin/env python3
"""Print a Markdown table of store-load runs from their run.json files.

  python3 -I bench/store-load/table.py RUN_DIR [RUN_DIR ...]
"""
from __future__ import annotations

import json
import sys
from pathlib import Path


def phase(reads: dict, name: str) -> str:
    p = reads.get(name, {})
    text = f"{p.get('p50_ms', 0):.0f} / {p.get('p99_ms', 0):.0f} ms, {p.get('reads_per_s', 0):.1f}/s"
    if p.get("failed"):
        text += f", **{p['failed']} of {p['calls']} failed**"
    elif name == "indexing":
        text += f", 0 of {p.get('calls', 0)} failed"
    return text


def main() -> None:
    print("| Run | Seed | Index | Reads before | Reads during indexing | Reads after | Peak RSS (MiB) | WAL | fsyncs |")
    print("|---|---|---|---|---|---|---|---|---|")
    for directory in sys.argv[1:]:
        r = json.loads((Path(directory) / "run.json").read_text())
        rss = r.get("peak_rss_mib", {})
        memory = f"server {rss.get('server', 0)}, indexer {rss.get('indexer', 0)}"
        if rss.get("postgres_pss"):
            memory += f", PostgreSQL PSS {rss['postgres_pss']} (RSS {rss['postgres_rss']})"
        wal = f"peak {r.get('wal_peak_mib', 0):.1f} MiB, end {r.get('wal_end_mib', 0):.1f} MiB"
        fs = r.get("fsyncs") or {}
        fsyncs = ", ".join(f"{k} {sum(v.values()) if isinstance(v, dict) else v}" for k, v in fs.items())
        pg = r.get("postgres_wal")
        if pg:
            fsyncs += f"; PostgreSQL WAL syncs {pg.get('wal_sync', 0)}"
        index = r.get("index", {})
        print(
            f"| {r.get('label', directory)} | {r['seed']['seconds']:.1f} s | {index.get('seconds', 0):.1f} s"
            f" | {phase(r['reads'], 'warmup')} | {phase(r['reads'], 'indexing')} | {phase(r['reads'], 'after')}"
            f" | {memory} | {wal} | {fsyncs} |"
        )


if __name__ == "__main__":
    main()
