#!/usr/bin/env python3
"""Mark every Rust file adapted from Graphify with a modification notice.

Apache-2.0 §4(b) asks that modified files carry a prominent notice; this
project gives it as one header line naming the upstream release. The
upstream files each Rust file ports are taken from the comparison receipts
(`migration/receipts/*.json`: `source` -> `rust_files`), so the header is the
same evidence qualification binds, not a hand-kept list. The resulting map is
saved as `migration/ADAPTED.json`, which a checkout without receipts (the public
export) checks against. Generated data tables keep their generator's
provenance line and are skipped.

Usage: mark_adapted_files.py [--check]
With --check nothing is written; files whose header is missing or stale are
listed and the exit status is non-zero.
"""
import argparse
import json
import sys
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MAP = ROOT / 'migration/ADAPTED.json'
MARK = '// Adapted from Graphify'
WIDTH = 100


def upstream_sources():
    """Rust file -> sorted upstream `graphify/*.py` files it ports."""
    ported = defaultdict(set)
    for receipt in sorted((ROOT / 'migration/receipts').glob('*.json')):
        try:
            record = json.loads(receipt.read_text())
        except (OSError, json.JSONDecodeError):
            continue
        source = record.get('source') if isinstance(record, dict) else None
        if not (isinstance(source, str) and source.startswith('graphify/')):
            continue
        for rust in record.get('rust_files') or {}:
            # The folder also keeps older receipts that name files of other
            # crates (src/identity.rs, src/impact.rs, src/retrieval.rs); only
            # this crate's files are mapped.
            if rust.startswith('src/') and rust.endswith('.rs') and (ROOT / rust).is_file():
                ported[rust].add(source)
    if not ported and MAP.is_file():
        return json.loads(MAP.read_text())['files']
    return {rust: sorted(sources) for rust, sources in ported.items()}


def header(sources, version, commit):
    lines = [f'{MARK} {version} ({commit[:8]}), Apache-2.0; see NOTICE.md.']
    current = '// Upstream:'
    for index, source in enumerate(sources):
        piece = f' {source}' + (',' if index < len(sources) - 1 else '')
        if len(current) + len(piece) > WIDTH:
            lines.append(current)
            current = '//          '
        current += piece
    lines.append(current)
    return lines


def strip(lines):
    """Drop an existing adaptation header (the mark line and its continuation)."""
    if not lines or not lines[0].startswith(MARK):
        return lines
    index = 1
    while index < len(lines) and lines[index].startswith(('// Upstream:', '//          ')):
        index += 1
    return lines[index:]


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    ledger = json.loads((ROOT / 'migration/FILES.json').read_text())
    version, commit = ledger['upstream_version'], ledger['upstream_commit']
    stale = []
    mapping = upstream_sources()
    if not args.check:
        MAP.write_text(json.dumps({'upstream_version': version, 'upstream_commit': commit,
                                   'files': dict(sorted(mapping.items()))}, indent=2) + '\n')
    for rust, sources in sorted(mapping.items()):
        path = ROOT / rust
        if not path.is_file():
            continue
        text = path.read_text()
        lines = text.split('\n')
        if lines and lines[0].startswith('// Generated'):
            continue
        wanted = '\n'.join(header(sources, version, commit) + strip(lines))
        if wanted != text:
            stale.append(rust)
            if not args.check:
                path.write_text(wanted)
    if args.check and stale:
        print('\n'.join(f'stale adaptation header: {rust}' for rust in stale))
        sys.exit(1)
    print(json.dumps({'files': len(mapping), 'updated' if not args.check else 'stale': len(stale)}))


if __name__ == '__main__':
    main()
