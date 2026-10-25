"""Recorded name translations for unchanged upstream test files.

Development only. The engine ships Graphicle names (tools/naming.py holds the
table). Some upstream tests drive the engine through the harness and name the
artifacts it reads or writes with the reference's spelling: a `.graphifyignore`
file the engine must honour, a `.graphify_root` marker, a skill folder or git
hook the engine wrote, the `<title>` of an exported page. For those tests,
like upstream_fixes.py does for oracle modules, this module applies recorded,
exact replacements to a temporary copy of the pinned archive:

- every replacement must match exactly its recorded number of times;
- the pinned checkout is never edited;
- each check names the table it uses, and its receipt records the table
  (parity.Native.receipt, interface_differences).

Only names are translated. Every assertion still checks the engine's exact
artifact under the engine's name.
"""
import contextlib
import shutil
import tempfile
from pathlib import Path

# check script stem -> upstream test file -> [(reference text, engine text, count)]
TABLES = {
    # Backups and Obsidian vault adoption run natively: the tests' sidecar,
    # manifest and tag names are the engine's.
    'check_export': {
        'tests/test_export.py': [
            ('.graphify_semantic_marker', '.graphicle_semantic_marker', 6),
            ('.graphify_labels.json', '.graphicle_labels.json', 3),
            # graph.json's writer stamp key (naming.NATIVE_FIELDS).
            ('graph_metadata["graphify_version"]', 'graph_metadata["graphicle_version"]', 1),
        ],
        'tests/test_obsidian_vault_migration.py': [
            ('.graphify_obsidian_manifest.json', '.graphicle_obsidian_manifest.json', 2),
            ('graphify/document', 'graphicle/document', 3),
            ('"I use graphify" in', '"I use graphicle" in', 1),
        ],
    },
    # Graph merges run natively: the committed project-root marker is the engine's.
    'check_build': {
        'tests/test_build_merge_hyperedges_and_prune.py': [
            ('(out / ".graphify_root")', '(out / ".graphicle_root")', 3),
        ],
        'tests/test_build_merge_shrink_guard.py': [
            ('(out / ".graphify_root")', '(out / ".graphicle_root")', 1),
        ],
    },
    # Discovery runs natively: the tests' ignore files and the detect result
    # field are the engine's.
    'check_detect': {
        'tests/test_detect.py': [
            # 0.9.92 (#4444) adds two of each (the valid-.git cases).
            ('.graphifyignore', '.graphicleignore', 55),
            ('graphifyignore_patterns', 'ignore_patterns', 12),
        ],
        'tests/test_ignore_file_encoding.py': [
            ('.graphifyignore', '.graphicleignore', 11),
        ],
    },
    # Git hooks installed natively: block markers, merge driver, rc file and
    # the baked setting carry the engine's names.
    'check_hooks': {
        'tests/test_hooks.py': [
            ('assert _HOOK_MARKER in', 'assert "# graphicle-hook-start" in', 2),
            ('count(_HOOK_MARKER)', 'count("# graphicle-hook-start")', 1),
            ('assert _CHECKOUT_MARKER in', 'assert "# graphicle-checkout-hook-start" in', 1),
            # 0.9.91 (#2418): the post-merge hook.
            ('assert _MERGE_HOOK_MARKER in', 'assert "# graphicle-merge-hook-start" in', 1),
            ('merge=graphify', 'merge=graphicle', 4),
            ('merge.graphify.driver', 'merge.graphicle.driver', 3),
            ('.graphifyrc', '.graphiclerc', 12),
            ('# graphify-hook-start', '# graphicle-hook-start', 2),
            ('# graphify-hook-end', '# graphicle-hook-end', 1),
            ('GRAPHIFY_VIZ_NODE_LIMIT', 'GRAPHICLE_VIZ_NODE_LIMIT', 23),
            # 0.9.92 (#4490): the post-rewrite hook, and the line a native hook
            # prints when it starts the background rebuild (test_hooks_rewrite.py
            # imports it from here).
            ('assert _REWRITE_HOOK_MARKER in', 'assert "# graphicle-rewrite-hook-start" in', 2),
            ('assert _REWRITE_HOOK_MARKER not in', 'assert "# graphicle-rewrite-hook-start" not in', 1),
            ('_LAUNCH_LINE = "launching background rebuild"', '_LAUNCH_LINE = "rebuild started"', 1),
        ],
        # 0.9.92 (#4490): real git operations against the native hooks; the first
        # build is the native `update` and the rebuild log is the engine's.
        'tests/test_hooks_rewrite.py': [
            ('graphify-out', '.graphicle', 11),
            ('GRAPHIFY_SKIP_HOOK', 'GRAPHICLE_SKIP_HOOK', 1),
            ('[sys.executable, "-m", "graphify", "update", "."]', '[os.environ["GRAPHICLE_GRAPH_BINARY"], "update", "."]', 1),
            ('".cache" / "graphify-rebuild.log"', '".cache" / "graphicle" / "rebuild.log"', 1),
        ],
    },
    # Command-line children run natively (check_cli.py CLI_FILES): the ignore
    # file, the persisted build options and the analysis and label sidecars
    # carry the engine's names.
    'check_cli': {
        'tests/test_extract_code_only_cli.py': [
            ('.graphifyignore', '.graphicleignore', 1),
            ('.graphify_build.json', '.graphicle_build.json', 1),
        ],
        'tests/test_cli_export.py': [
            ('.graphify_analysis.json', '.graphicle_analysis.json', 14),
            ('.graphify_labels.json', '.graphicle_labels.json', 5),
        ],
    },
    # The native page's <title> brand.
    'check_export_html': {
        'tests/test_export.py': [
            ('title.startswith("graphify - ")', 'title.startswith("Graphicle - ")', 1),
            ('label = title[len("graphify - "):]', 'label = title[len("Graphicle - "):]', 1),
        ],
        # The theme choice is stored under the engine's key (0.9.87, #3894).
        'tests/test_html_theme.py': [
            ('graphify-theme', 'graphicle-theme', 2),
        ],
    },
}

_applied = {}


def table(check):
    return TABLES.get(check, {})


def applied():
    """The tables applied in this process, for the receipt."""
    return {check: {path: [list(r) for r in rows] for path, rows in tables.items()}
            for check, tables in _applied.items()}


def _translate(root, check):
    for relative, replacements in table(check).items():
        path = root / relative
        text = path.read_text(encoding='utf-8')
        for before, after, count in replacements:
            found = text.count(before)
            if found != count:
                raise AssertionError(f'{check}: {relative}: {before!r} occurs {found} times, recorded {count}')
            text = text.replace(before, after)
        # The copy keeps the oracle's modes; a read-only oracle tree must still
        # yield a writable copy.
        path.chmod(path.stat().st_mode | 0o200)
        path.write_text(text, encoding='utf-8')
    if table(check):
        _applied[check] = table(check)


@contextlib.contextmanager
def isolated(paths, check=None):
    """The pinned archive copied to a temporary root, with the check's table applied.

    Yields the given test paths (optionally with ::selectors) inside the copy.
    Copying the whole ledger keeps package data, fixtures and conftest.py
    beside the tests; `import graphify` still resolves to the pinned checkout.
    """
    from parity import LEDGER, SOURCE
    with tempfile.TemporaryDirectory(prefix='graphify-upstream-') as tmp:
        root = Path(tmp)
        for row in LEDGER['files']:
            src = SOURCE / row['path']
            dst = root / row['path']
            dst.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(src, dst)
        # An oracle tree newer than the recorded ledger (a release being ported
        # before the ledger is re-pinned) has files the ledger does not list:
        # new tests, and package data or tool fragments they read. They are
        # copied too, so the copy mirrors the oracle tree.
        for src in sorted(SOURCE.rglob('*')):
            dst = root / src.relative_to(SOURCE)
            if src.is_file() and not dst.exists():
                dst.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(src, dst)
        _translate(root, check)
        yield root, [str(root / p) for p in paths]
