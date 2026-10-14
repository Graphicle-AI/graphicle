#!/usr/bin/env python3
"""`extract` action `incremental` must equal a full rebuild after every change.

Incremental extraction re-parses only files whose content hash misses the
extraction cache and feeds the cached per-file records of every other file
through the same cross-file `assemble` as a cold build. This check edits a
fixture repository step by step (signature change, new symbol, deleted file,
rename, edit of a file nobody imports) and after each step compares the native
incremental result with a fresh native full extraction and with the pinned
Python `extract` over the same tree. It also reports which files were
re-parsed, and records the pinned `merge_raw_extraction` (the shipped
incremental path) against the full rebuild to document the INFERRED-edge gap
it leaves behind (#2230), which the incremental action does not.
"""
import json
import shutil
import tempfile
from pathlib import Path
from parity import Native, ROOT
import graphify.extract as ref
import graphify.build as build

native = Native('subset'); counts = {}

FILES = {
    'pkg/__init__.py': '',
    'pkg/core.py': (
        'def compute(values):\n    return sum(values)\n\n\n'
        'class Engine:\n    def run(self, values):\n        return compute(values)\n'
    ),
    'pkg/service.py': (
        'from pkg.core import compute, Engine\n\n\n'
        'class Service:\n    def start(self, values):\n        engine = Engine()\n'
        '        return compute(values) + engine.run(values)\n'
    ),
    'pkg/cli.py': (
        'from pkg.service import Service\n\n\n'
        'def main():\n    return Service().start([1, 2, 3])\n'
    ),
    'pkg/standalone.py': 'def lonely():\n    return 1\n',
    'web/lib.js': 'export function render(node) {\n  return node.id;\n}\n',
    'web/app.js': "import { render } from './lib.js';\n\nexport function main() {\n  return render({ id: 1 });\n}\n",
    'README.md': '# Fixture\n\n`compute` sums values; `Service.start` drives the `Engine`.\n',
}

def canonical(value):
    value = json.loads(json.dumps(value, sort_keys=True))
    for key in ('nodes', 'edges'):
        if isinstance(value.get(key), list):
            value[key].sort(key=lambda item: json.dumps(item, sort_keys=True))
    return value

def write(root, files):
    for relative, text in files.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

def listing(root):
    return sorted(str(p) for p in root.rglob('*') if p.is_file())

def reference_full(root, paths):
    cache = Path(tempfile.mkdtemp(prefix='graphify-subset-ref-'))
    try:
        return ref.extract([Path(p) for p in paths], cache_root=cache, root=root, parallel=False)
    finally:
        shutil.rmtree(cache, ignore_errors=True)

def native_full(root, paths):
    cache = tempfile.mkdtemp(prefix='graphify-subset-cold-')
    try:
        return native.result('extract', paths=paths, root=str(root), cache_root=cache, use_cache=True, parallel=False)
    finally:
        shutil.rmtree(cache, ignore_errors=True)

def compare(tag, root, cache, paths, expected_changed):
    counts[tag] = counts.get(tag, 0) + 1
    incremental = native.result('extract', action='incremental', paths=paths, root=str(root),
                                cache_root=cache, use_cache=True, parallel=False)
    report = incremental['report']
    got = canonical(incremental['extraction'])
    full = canonical(native_full(root, paths))
    reference = canonical(reference_full(root, paths))
    changed = sorted(Path(p).relative_to(root).as_posix() for p in report['changed'])
    supported = [p for p in paths if native.result('extract', action='extractor', path=p)]
    # The pinned engine never caches these suffixes, so they are re-parsed on
    # every run regardless of content.
    bypass = [Path(p).relative_to(root).as_posix() for p in supported if Path(p).suffix in ref._JS_CACHE_BYPASS_SUFFIXES]
    expected_changed = sorted(set(expected_changed) | set(bypass))
    if got != full:
        native.fail(tag=tag, reason='incremental differs from native full rebuild', report=report, incremental=got, full=full)
    if got != reference:
        native.fail(tag=tag, reason='incremental differs from pinned extract', report=report, incremental=got, reference=reference)
    if changed != sorted(expected_changed):
        native.fail(tag=tag, reason='unexpected re-extraction set', changed=changed, expected=sorted(expected_changed))
    if report['unchanged'] != len(supported) - len(expected_changed) or report['unsupported'] != len(paths) - len(supported):
        native.fail(tag=tag, reason='report counts do not partition the inputs', report=report, supported=len(supported), inputs=len(paths))
    return got

def merge_gap(root, before, paths_after, changed_relative, full_after):
    """Pinned shipped path: re-extract only the changed files and merge forward."""
    directory = Path(tempfile.mkdtemp(prefix='graphify-subset-merge-'))
    try:
        graph_path = directory / 'graph.json'
        graph_path.write_text(json.dumps({'nodes': before['nodes'], 'edges': before['edges'], 'directed': True}))
        changed = [Path(p) for p in paths_after if Path(p).relative_to(root).as_posix() in changed_relative]
        new = reference_full(root, changed)
        merged = canonical(build.merge_raw_extraction(new, graph_path, root=root))
    finally:
        shutil.rmtree(directory, ignore_errors=True)
    def inferred(extraction):
        return {json.dumps(e, sort_keys=True) for e in extraction['edges'] if e.get('confidence') == 'INFERRED'}
    merged_inferred, full_inferred = inferred(merged), inferred(full_after)
    return {
        'stale_inferred_edges': len(merged_inferred - full_inferred),
        'missing_inferred_edges': len(full_inferred - merged_inferred),
        'merged_edges': len(merged['edges']),
        'full_edges': len(full_after['edges']),
    }

gaps = []
try:
    with tempfile.TemporaryDirectory(prefix='graphify-subset-') as directory:
        root = Path(directory).resolve() / 'repo'
        cache = str(Path(directory).resolve() / 'cache')
        write(root, FILES)
        paths = listing(root)
        native.use_working_directory(root)
        supported = [p for p in FILES if native.result('extract', action='extractor', path=str(root / p))]
        cold = compare('cold', root, cache, paths, supported)
        # Same tree again: nothing re-parsed, same graph.
        compare('unchanged', root, cache, paths, [])
        # Signature change used across files: the importer must be re-resolved
        # even though only core.py and service.py changed on disk.
        renamed = dict(FILES)
        renamed['pkg/core.py'] = FILES['pkg/core.py'].replace('compute(values)', 'compute_total(values)').replace('return compute(', 'return compute_total(')
        renamed['pkg/service.py'] = FILES['pkg/service.py'].replace('compute', 'compute_total')
        write(root, renamed)
        after = compare('signature-change', root, cache, paths, ['pkg/core.py', 'pkg/service.py'])
        gaps.append({'step': 'signature-change', **merge_gap(root, cold, paths, {'pkg/core.py'}, after)})
        # New symbol in one file only.
        added = dict(renamed)
        added['pkg/core.py'] += '\n\ndef helper():\n    return compute_total([1])\n'
        write(root, added)
        before = after
        after = compare('new-symbol', root, cache, paths, ['pkg/core.py'])
        gaps.append({'step': 'new-symbol', **merge_gap(root, before, paths, {'pkg/core.py'}, after)})
        # Edit a file nobody imports: only it is re-parsed.
        added['pkg/standalone.py'] = 'def lonely():\n    return 2\n'
        write(root, added)
        compare('isolated-edit', root, cache, paths, ['pkg/standalone.py'])
        # Delete a file: the input set shrinks, nothing else is re-parsed.
        (root / 'pkg/cli.py').unlink()
        paths = listing(root)
        compare('delete', root, cache, paths, [])
        # Rename a file: the new path misses the cache (the hash is salted with
        # the relative path), the old one disappears from the inputs.
        (root / 'web/lib.js').rename(root / 'web/render.js')
        (root / 'web/app.js').write_text(FILES['web/app.js'].replace('./lib.js', './render.js'))
        paths = listing(root)
        compare('rename', root, cache, paths, ['web/app.js', 'web/render.js'])
        # Cache disabled: everything is re-parsed and the graph is unchanged.
        disabled = native.result('extract', action='incremental', paths=paths, root=str(root), cache_root=cache, use_cache=False, parallel=False)
        counts['cache-disabled'] = 1
        if len(disabled['report']['changed']) != len(paths) - disabled['report']['unsupported'] or disabled['report']['unchanged'] != 0:
            native.fail(tag='cache-disabled', reason='cache disabled must re-extract everything', report=disabled['report'])
        if canonical(disabled['extraction']) != canonical(native_full(root, paths)):
            native.fail(tag='cache-disabled', reason='cache-disabled incremental differs from full')
finally:
    native.close()

native.receipt('graphify/extract.py', ['src/extract.rs', 'src/cache.rs', 'src/cache_paths.rs', 'src/extractors/resolution.rs', 'src/extractors/parse_cache.rs'],
    checked=counts, shipped_merge_gap=gaps,
    comparisons='After every edit of the fixture repository (signature change used across files, new symbol, edit of an unimported file, deletion, rename, cache disabled) the native `extract` action `incremental` equals both a fresh native full extraction and the pinned Python `extract` over the same tree, and its report names exactly the files whose content hash missed the extraction cache plus the suffixes the pinned engine never caches, with unchanged/unsupported counts partitioning the inputs.',
    scope='The incremental action is additive: it runs the pinned extraction with the cache on and returns the cache traffic beside the result. Soundness follows from re-running the whole cross-file resolution over cached per-file records, so INFERRED edges in unchanged files are regenerated. `shipped_merge_gap` records how many INFERRED edges the pinned merge_raw_extraction path (re-extract changed files, merge forward) leaves stale or missing against the full rebuild on the same steps; it documents the baseline, it is not a comparison the native code must match.')
