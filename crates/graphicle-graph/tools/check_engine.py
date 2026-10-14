"""Compare the native generic engine with pinned Graphify; no runtime Python."""
import dataclasses
import json
import os
import shutil
import atexit
import sys
import tempfile
import threading
from pathlib import Path
from parity import Native, ROOT, SOURCE, upstream_tests
import graphify.extract as reference
import graphify.extractors.engine as original_engine
from upstream_fixes import oracle_source
# The oracle is the pinned engine with the reviewed adaptations applied in memory.
exec(compile(oracle_source('graphify/extractors/engine.py'), '<pinned engine.py with reviewed fixes>', 'exec'), original_engine.__dict__)

native = Native('engine')
original = original_engine._extract_generic
live_differences = []
atexit.register(lambda: (ROOT/'migration/receipts/engine-live-differences.json').write_text(json.dumps(live_differences, indent=2)+'\n'))
config_names = {id(v): k for k, v in vars(reference).items() if isinstance(v, reference.LanguageConfig)}

def config_json(config):
    out = {}
    for field in dataclasses.fields(config):
        value = getattr(config, field.name)
        if callable(value):
            out.setdefault('_callbacks', {})[field.name] = value.__name__
        else:
            out[field.name] = sorted(value) if isinstance(value, (set, frozenset)) else value
    return out

# Upstream drives _extract_generic from pool threads too (0.9.87's
# test_the_pool_hands_its_workers_the_overrides runs extract() on a
# ThreadPoolExecutor); the one native JSON Lines process answers one request
# at a time, so each substituted call holds it for its request and response.
native_lock = threading.Lock()

def extract(path, config, *, source_override=None, scan_root=None):
    with native_lock:
        native.use_working_directory(Path.cwd())
        actual = native.result('engine', path=str(path), config=config_json(config),
                             source=source_override.decode('utf-8', errors='replace') if source_override is not None else None,
                             scan_root=str(scan_root) if scan_root is not None else None)
    if '--compare-all' in sys.argv:
        expected = original(path, config, source_override=source_override, scan_root=scan_root)
        if canonical(expected) != canonical(actual):
            live_differences.append({'path': str(path), 'config': config_json(config), 'source': source_override.decode('utf-8',errors='replace') if source_override is not None else Path(path).read_text(errors='replace') if Path(path).is_file() else None, 'expected': json.loads(json.dumps(expected)), 'actual': json.loads(json.dumps(actual))})
    return actual

SAMPLES = {
    'PYTHON': ('py', 'from typing import List\nclass Item: pass\nclass Base: pass\nclass Worker(Base):\n    def run(self, x: Item) -> Item:\n        return helper(x)\ndef helper(x): return x\ndef outer(value):\n    def nested(): return helper(value)\n    return nested()\n'),
    'JS': ('js', 'export class Worker { run(x) { return helper(x); } }\nexport function helper(x) { return x; }\nexport const closure = x => helper(x);\n'),
    'TS': ('ts', 'export interface Work { run(x: Item): Item; }\nexport class Item {}\nexport class Worker implements Work { run(x: Item): Item { return helper(x); } }\nexport function helper(x: Item): Item { return x; }\n'),
    'TSX': ('tsx', 'import React from "react";\nexport function App() { return <div onClick={() => helper()}/>; }\nfunction helper() {}\n'),
    'JAVA': ('java', 'package app;\nimport app.Item;\nclass Item {}\nclass Worker { Item item; Item run(Item x) { return helper(x); } Item helper(Item x) { return x; } }\n'),
    'GROOVY': ('groovy', 'package app\nclass Worker { def run() { helper() } def helper() { return 1 } }\n'),
    'C': ('c', 'int helper(int x) { return x; }\nint run(void) { return helper(2); }\n'),
    'CPP': ('cpp', 'class Item {};\nclass Worker { Item value; public: Item run(Item x) { return helper(x); } Item helper(Item x) { return x; } };\n'),
    'RUBY': ('rb', 'module App\n class Item\n  def run(x)\n   helper(x)\n  end\n  def helper(x)\n   x\n  end\n end\nend\n'),
    'CSHARP': ('cs', 'namespace App { class Item {} class Worker { Item item; public Item Run(Item x) { return Helper(x); } Item Helper(Item x) { return x; } } }\n'),
    'KOTLIN': ('kt', 'package app\nclass Item\nclass Worker { fun run(x: Item): Item { return helper(x) }\n fun helper(x: Item): Item { return x } }\n'),
    'SCALA': ('scala', 'class Item\nclass Worker { def run(x: Item): Item = helper(x)\n def helper(x: Item): Item = x }\n'),
    'PHP': ('php', '<?php\nclass Item {}\nclass Worker { public Item $item; public function run(Item $x): Item { return helper($x); } }\nfunction helper(Item $x): Item { return $x; }\n'),
    'LUA': ('lua', 'local util = require("util")\nfunction helper(x) return x end\nfunction run() return helper(2) end\n'),
    'SWIFT': ('swift', 'struct Item {}\nclass Worker { var item: Item = Item()\n func run(_ x: Item) -> Item { return helper(x) }\n func helper(_ x: Item) -> Item { return x } }\n'),
}

def canonical(value):
    value = json.loads(json.dumps(value))
    for key in ('nodes', 'edges'):
        if key in value:
            value[key].sort(key=lambda x: json.dumps(x, sort_keys=True))
    return value

failures = []
comparisons = 0
with tempfile.TemporaryDirectory(prefix='graphify-native-engine-') as folder:
    for language, (suffix, source) in SAMPLES.items():
        config = getattr(reference, f'_{language}_CONFIG')
        path = Path(folder) / f'example.{suffix}'
        path.write_text(source)
        for tag, content in [('source', source), ('empty', ''), ('unicode', source + '\n// π 🧪\n')]:
            expected = original(path, config, source_override=content.encode())
            actual = extract(path, config, source_override=content.encode())
            comparisons += 1
            if canonical(expected) != canonical(actual):
                failures.append({'language': language, 'case': tag, 'expected': json.loads(json.dumps(expected)), 'actual': json.loads(json.dumps(actual))})
    report = {'comparisons': comparisons, 'failures': failures}
    (ROOT/'migration/receipts/engine-comparisons.json').write_text(json.dumps(report, indent=2)+'\n')
print(json.dumps({'comparisons': comparisons, 'failed': len(failures), 'cases': [(f['language'],f['case']) for f in failures]}), flush=True)
if '--samples-only' in sys.argv:
    native.close()
    raise SystemExit(bool(failures))
reference._extract_generic = extract
original_engine._extract_generic = extract
paths = [str(p.relative_to(SOURCE)) for p in sorted((SOURCE/'tests').glob('test_*.py')) if any(word in p.read_text() for word in ('extract_python', 'extract_js', 'extract_java', 'extract_groovy', 'extract_cpp', 'extract_csharp', 'extract_kotlin', 'extract_scala', 'extract_php', 'extract_lua', 'extract_swift', 'extract_ruby', '_extract_generic'))]
# Java super/inherited calls (0.9.73) are driven through extract() without naming an extractor.
paths += [p for p in ['tests/test_java_member_calls.py'] if p not in paths and (SOURCE / p).is_file()]
# 0.9.78-0.9.88 contracts that drive extract() without naming an extractor, run
# with the native per-file engine (same list as check_extract.py).
EXTRACT_0988 = [
    'tests/test_python_typed_receiver_calls.py', 'tests/test_python_self_type_ref.py', 'tests/test_python_forward_ref_file_id.py',
    'tests/test_python_nested_class_identity.py', 'tests/test_python_reexport_type_binding.py', 'tests/test_qualified_annotation_stub_bind.py',
    'tests/test_python_decorators.py', 'tests/test_csharp_member_chain_calls.py', 'tests/test_csharp_var_call_return_type.py',
    'tests/test_csharp_verbatim_interpolation.py', 'tests/test_csharp_enum_members.py', 'tests/test_pr10_csharp_overrides.py',
    'tests/test_java_anonymous_class.py', 'tests/test_php_static_call_resolution.py', 'tests/test_php_object_creation.py',
    'tests/test_swift_self_type_ref.py', 'tests/test_ts_export_type_star.py', 'tests/test_ts_receiver_member_calls.py',
    'tests/test_loose_sibling_import_resolution.py', 'tests/test_c_include_resolution.py', 'tests/test_source_file_encoding.py',
    'tests/test_extract_path_memo.py', 'tests/test_resolve_memoization.py', 'tests/test_partial_extraction_warning.py',
    'tests/test_no_cluster_run_only_keys.py::test_run_only_keys_are_the_ones_extract_returns',
]
paths += [p for p in EXTRACT_0988 if p not in paths and (SOURCE / p.split('::')[0]).is_file()]
if '--test' in sys.argv:
    paths = [sys.argv[sys.argv.index('--test') + 1]]
# An enclosing development checkout ignores research/. Isolate the unchanged
# pinned archive so fixture discovery sees the upstream project's ignore rules.
with tempfile.TemporaryDirectory(prefix='graphify-engine-upstream-') as tmp:
    isolated = Path(tmp)
    from parity import LEDGER
    for row in LEDGER['files']:
        src = SOURCE / row['path']
        dst = isolated / row['path']
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(src, dst)
    import pytest
    class Reports:
        passed = 0
        skipped = 0
        def pytest_runtest_logreport(self, report):
            if report.when == 'call' and report.passed: self.passed += 1
            if report.skipped: self.skipped += 1
    reports = Reports()
    deselected = str(isolated/'tests/test_extract.py')+'::test_extract_generic_surfaces_tree_sitter_version_mismatch_hint'
    # The per-file engine prints its decode warning in the native process; the
    # test counts what this process prints, so only the reference's later passes
    # are counted there. check_extract runs it, and cli/tests/hook_nudge_budget.rs
    # (extract_warns_once_per_file_that_is_not_utf8) checks the native count.
    mixed = {'test_cp1252_file_warns_once_through_every_pass': 'counts the warnings the test process prints; the substituted per-file engine prints its own in the native process'}
    result = pytest.main(['-q','-p','no:cacheprovider','-k',' and '.join(['not test_extract_generic_surfaces_tree_sitter_version_mismatch_hint',*('not '+name for name in mixed)]),*[str(isolated/p) for p in paths]],plugins=[reports])
    tests = {'paths':paths,'passed':reports.passed,'skipped':reports.skipped,'python_abi_test_not_applicable':deselected.split('::')[-1],'not_applicable_in_this_check':mixed}
    assert result == 0, f'Upstream tests failed: {result}'
assert not failures and not live_differences, 'Differential engine comparisons failed'
native.receipt('graphify/extractors/engine.py',
               [str(p.relative_to(ROOT)) for p in sorted((ROOT/'src/extractors').glob('engine*.rs'))] + ['src/assets/engine/sets.json', 'src/assets/engine/configs.json'],
               upstream_tests=tests, differential_cases=comparisons,
               interface_differences=['Parsers are statically linked; custom LanguageConfig callbacks use Rust closures.'])
native.close()
