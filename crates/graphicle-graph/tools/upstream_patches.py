#!/usr/bin/env python3
"""Write the reviewed upstream fixes as patches against the pinned Graphify source.

Every module the development oracle adapts (tools/upstream_fixes.py) becomes
one unified diff of the pinned file against its adapted text, headed by the
reviewed behaviours recorded for it in migration/upstream-review/adaptations.json.
Each patch is checked to apply cleanly (`git apply --check`) to a copy of the
pinned file. Behaviours enforced only by native regression tests have no oracle
text and are listed in index.json without a patch. Nothing is submitted
anywhere: the patches exist so the fixes can be offered upstream later.

Usage: upstream_patches.py [--out migration/upstream-patches]
"""
import argparse
import difflib
import json
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'tools'))
from parity import SOURCE  # noqa: E402
import upstream_fixes  # noqa: E402

ADAPTATIONS = ROOT / 'migration/upstream-review/adaptations.json'
PIN = re.search(r'Pinned: (Graphify \S+ \([0-9a-f]+\))', upstream_fixes.__doc__).group(1)


def applies(relative, patch):
    with tempfile.TemporaryDirectory(prefix='graphify-patch-') as tmp:
        target = Path(tmp) / relative
        target.parent.mkdir(parents=True)
        shutil.copy2(SOURCE / relative, target)
        subprocess.run(['git', 'init', '-q', tmp], check=True)
        result = subprocess.run(['git', 'apply', '--check', '-'], cwd=tmp, input=patch,
                                text=True, capture_output=True)
        return result.returncode == 0, result.stderr.strip()


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--out', type=Path, default=ROOT / 'migration/upstream-patches')
    args = ap.parse_args()
    reviewed = json.loads(ADAPTATIONS.read_text())
    modules = sorted(set(reviewed) | set(upstream_fixes.adapted_modules()))
    args.out.mkdir(parents=True, exist_ok=True)
    for old in args.out.glob('*.patch'):
        old.unlink()
    index, failures = [], []
    for n, relative in enumerate(modules, 1):
        behaviours = reviewed.get(relative, [])
        entry = {'module': relative, 'behaviours': [
            {k: b[k] for k in ('url', 'behavior', 'kind', 'native_regression') if k in b} for b in behaviours]}
        pinned = (SOURCE / relative).read_text()
        try:
            adapted = upstream_fixes.oracle_source(relative)
        except ValueError:
            adapted = pinned
        if adapted == pinned:
            entry['patch'] = None
            entry['note'] = 'the oracle keeps this module unchanged; the Rust behaviour is covered by native tests'
            index.append(entry)
            continue
        diff = ''.join(difflib.unified_diff(pinned.splitlines(keepends=True), adapted.splitlines(keepends=True),
                                            f'a/{relative}', f'b/{relative}'))
        header = [f'# Reviewed fix for {relative} against {PIN}.']
        for b in behaviours:
            ref = b.get('url') or b.get('native_regression') or ''
            header.append(f'# - {b.get("behavior", "").strip()} {f"({ref})" if ref else ""}'.rstrip())
        patch = '\n'.join(header) + f'\ndiff --git a/{relative} b/{relative}\n' + diff
        ok, error = applies(relative, patch)
        if not ok:
            failures.append((relative, error))
        name = f'{n:02d}-{relative.removeprefix("graphify/").replace("/", "-").removesuffix(".py")}.patch'
        (args.out / name).write_text(patch)
        entry.update({'patch': name, 'applies_to_pin': ok,
                      'added': sum(1 for l in diff.splitlines() if l.startswith('+') and not l.startswith('+++')),
                      'removed': sum(1 for l in diff.splitlines() if l.startswith('-') and not l.startswith('---'))})
        index.append(entry)
    (args.out / 'index.json').write_text(json.dumps({
        'pinned_source': PIN,
        'behaviours': sum(len(e['behaviours']) for e in index),
        'patches': sum(1 for e in index if e['patch']),
        'submitted': False,
        'modules': index,
    }, indent=2) + '\n')
    for relative, error in failures:
        print(f'does not apply: {relative}: {error}', file=sys.stderr)
    print(f"{sum(1 for e in index if e['patch'])} patch(es), "
          f"{sum(len(e['behaviours']) for e in index)} reviewed behaviour(s) -> {args.out}")
    return 1 if failures else 0


if __name__ == '__main__':
    sys.exit(main())
