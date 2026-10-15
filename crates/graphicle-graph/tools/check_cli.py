"""Compare native CLI bookkeeping and guard contracts with pinned Python tests.

Full executable workflows are qualified separately by check_native_workflows.py.
"""
import ast, contextlib, copy, functools, inspect, io, json, os, sys, tempfile
from pathlib import Path
from parity import Native, ROOT, SOURCE, BINARY
from isolated_upstream import run
import naming
import subprocess
import graphify.cli as ref
import graphify.__main__ as mainmod

native = Native('cli')
counts = {}
inside = False

def serial(v):
    if isinstance(v, Path): return str(v)
    if isinstance(v, dict): return {str(k): serial(x) for k, x in v.items()}
    if isinstance(v, (list, tuple, set, frozenset)): return [serial(x) for x in v]
    return v

actions = {
    '_stamped_manifest_files': ('stamped', {'files_by_type':'files','sem_result':'semantic','root':'root','partial_source_files':'partial','failed_ast_sources':'failed','unverified_semantic_sources':'unverified'}),
    '_stale_graph_sources': ('stale', {'graph_path':'graph','scan_root':'root','seen_files':'seen','detection':'detection'}),
    '_zero_node_stamped_code_sources': ('zero_sources', {'graph_path':'graph','scan_root':'root','unchanged_code':'unchanged'}),
    '_zero_node_stamped_semantic_sources': ('zero_sources', {'graph_path':'graph','scan_root':'root','unchanged_semantic':'unchanged'}),
    '_bash_invokes_search': ('bash_search', {'cmd_str':'command'}),
    '_is_cwd_relative': ('cwd_relative', {'value':'path'}),
    '_target_is_indexed': ('target_indexed', {'file_path':'path','root':'root'}),
}

for name, (action, fields) in actions.items():
    original = getattr(ref, name); signature = inspect.signature(original)
    @functools.wraps(original)
    def wrapper(*args, _name=name, _action=action, _fields=fields, _original=original, _signature=signature, **kwargs):
        global inside
        if inside: return _original(*args, **kwargs)
        bound = _signature.bind(*args, **kwargs); bound.apply_defaults()
        request = {'action': _action, **{dest: serial(bound.arguments[src]) for src, dest in _fields.items()}}
        if _name == '_zero_node_stamped_semantic_sources': request['semantic'] = True
        if _name == '_is_cwd_relative': request['windows'] = os.name == 'nt'
        inside = True
        try: expected = _original(*args, **kwargs)
        finally: inside = False
        if _name != '_is_cwd_relative': native.use_working_directory(Path.cwd())
        actual = native.result('cli', **request)
        if _name == '_stale_graph_sources': actual = actual['sources']
        counts[_name] = counts.get(_name, 0) + 1
        if actual != serial(expected): native.fail(name=_name, request=request, expected=serial(expected), actual=actual)
        return actual
    setattr(ref, name, wrapper)
    if hasattr(mainmod, name): setattr(mainmod, name, wrapper)

# _prune_graph_json_sources rewrites graph.json in place: the reference runs on
# a copy, the native on the file itself; the count and the bytes must match.
original_prune = ref._prune_graph_json_sources
@functools.wraps(original_prune)
def prune(graph_path, stale_sources):
    graph_path = Path(graph_path)
    with tempfile.TemporaryDirectory(prefix='graphify-prune-') as tmp:
        twin = Path(tmp) / graph_path.name
        if graph_path.exists(): twin.write_bytes(graph_path.read_bytes())
        expected = original_prune(twin, stale_sources)
        expected_bytes = twin.read_bytes() if twin.exists() else None
    native.use_working_directory(Path.cwd())
    actual = native.result('cli', action='prune', graph=str(graph_path), stale=[str(s) for s in stale_sources])
    counts['_prune_graph_json_sources'] = counts.get('_prune_graph_json_sources', 0) + 1
    actual_bytes = graph_path.read_bytes() if graph_path.exists() else None
    if actual != expected or actual_bytes != expected_bytes:
        native.fail(name='_prune_graph_json_sources', stale=list(map(str, stale_sources)), expected=[expected, expected_bytes and expected_bytes.decode('utf-8', 'replace')], actual=[actual, actual_bytes and actual_bytes.decode('utf-8', 'replace')])
    return actual
ref._prune_graph_json_sources = prune
if hasattr(mainmod, '_prune_graph_json_sources'): mainmod._prune_graph_json_sources = prune

def selected_tests(names):
    selected = []
    for path in sorted((SOURCE/'tests').glob('test_*.py')):
        text = path.read_text()
        if not any(name in text for name in names): continue
        module = ast.parse(text)
        def visit(body, prefix=''):
            for node in body:
                if isinstance(node, ast.ClassDef): visit(node.body, prefix+node.name+'::')
                elif isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name.startswith('test_'):
                    source = ast.get_source_segment(text, node) or ''
                    if any(name in source for name in names): selected.append('tests/'+path.name+'::'+prefix+node.name)
        visit(module.body)
    return selected

# The assistant hook guard (#522; 0.9.84-0.9.88 nudge budgets, #3435) runs on
# the native executable: `hook-guard <kind> [--strict]` with the test's stdin,
# working directory, environment and output folder (the reference reads its
# graphify.paths.GRAPHIFY_OUT, which the tests patch). Its stdout is mapped back
# to the reference names; it must exit 0 (fail open).
GUARD_FILES = ['tests/test_hook_guard.py', 'tests/test_hook_strict.py']
GUARD_NOT_APPLICABLE = {
    'test_search_out_path_error_is_swallowed': "injects an OSError into the reference's graphify.paths.out_path; the native guard has no Python seam to fail (its own errors fail open the same way)",
    'test_gemini_always_allows_even_when_check_throws': "injects an OSError into the reference's graphify.paths.out_path; the native gemini guard has no Python seam to fail (it always prints the allow decision)",
    # 0.9.89 (#4368): the hint's graph-size cap is a compiled constant in the
    # native guard (24 MiB, as upstream); cli/tests/hook_guard_hints.rs checks
    # it at the cap and one byte over (a_graph_over_the_size_cap_keeps_the_plain_nudge).
    'test_oversized_graph_keeps_the_plain_nudge': "patches the reference's cli._HINT_GRAPH_MAX_BYTES to 10 bytes; the native cap is a compiled constant with no seam (cli/tests/hook_guard_hints.rs checks it at 24 MiB)",
}
# Upstream files that run the command line as a child process (`python -m
# graphify ...`), first run here on 14 October 2026: every such child runs the
# native executable (upstream_cli.native_graphify). Each file runs in its own
# session, so its receipt entry counts the native runs it made.
# (tests/test_cli_broken_pipe.py starts its children with subprocess.Popen,
# which this adapter does not route, so it is not run here.)
CLI_FILES = ['tests/test_cli_help.py', 'tests/test_cargo_missing_manifest.py', 'tests/test_extract_code_only_cli.py',
             'tests/test_merge_graphs_cli.py', 'tests/test_gemini_hook.py', 'tests/test_read_hook.py',
             'tests/test_cli_export.py']
# Known differences (the engine fails the test) and tests that check something
# the engine does not have, with the reason (isolated_upstream.run).
CLI_DIFFERENCES = {
    'tests/test_cli_help.py::test_help_lists_prs_provider_and_export_formats':
        "The engine's `--help` text is its own: it does not list the `prs` command, it names the export formats "
        "in one list instead of `export <format>` lines, and it lists the `hook-guard` and `hook-check` internals "
        "that the reference hides.",
    'tests/test_extract_code_only_cli.py::test_code_only_succeeds_without_key':
        "`extract --code-only` with no API key builds the code graph, but the engine does not print the "
        "reference's `--code-only: skipping` line.",
    'tests/test_extract_code_only_cli.py::test_extract_usage_advertises_code_only':
        "`extract` with no path prints the engine's error `must specify a path to scan or a --postgres DSN` "
        "instead of the reference's usage text, so `--code-only` is not mentioned.",
    'tests/test_extract_code_only_cli.py::test_extract_names_skipped_sensitive_files':
        "The engine's `extract` does not print the reference's line that names the files it skipped as "
        "potentially sensitive.",
    'tests/test_extract_code_only_cli.py::test_code_only_force_preserves_existing_semantic_layer':
        "`extract --force --code-only` keeps the existing semantic layer (the test's checks of the graph pass), "
        "but the engine does not print the reference's `existing semantic layer preserved` line.",
    'tests/test_cli_export.py::test_cluster_only_warns_when_labeling_flags_are_ignored':
        "`cluster-only` with saved labels and `--backend` exits 0 without the reference's warning on stderr "
        "that the labeling flags are ignored.",
}
CLI_NOT_APPLICABLE = {
}
original_guard = ref._run_hook_guard
original_run = subprocess.run
@functools.wraps(original_guard)
def native_hook_guard(kind, strict=False):
    import graphify.paths as paths
    data = b'' if kind == 'gemini' else sys.stdin.buffer.read()
    env = naming.native_env(os.environ, 'upstream')
    env[naming.NATIVE_ENV + 'OUT'] = str(paths.GRAPHIFY_OUT)
    argv = [str(BINARY), 'hook-guard', kind] + (['--strict'] if strict else [])
    result = original_run(argv, input=data, cwd=os.getcwd(), env=env, capture_output=True, timeout=60)
    counts['hook-guard'] = counts.get('hook-guard', 0) + 1
    if result.returncode:
        native.fail(name='hook-guard', kind=kind, strict=strict, returncode=result.returncode, stdin=data.decode('utf-8', 'replace'), stderr=result.stderr.decode('utf-8', 'replace'))
    sys.stdout.write(naming.to_upstream(result.stdout.decode('utf-8'), 'upstream'))

if __name__ == '__main__':
    from upstream_cli import native_graphify
    paths = selected_tests(actions)
    # 0.9.86 (#3413): a pruned graph.json is written as one compact line.
    paths += ['tests/test_compact_graph_json.py::test_prune_output_is_one_line']
    guard = [p for p in GUARD_FILES if (SOURCE/p).is_file()]
    cli_files = [p for p in CLI_FILES if (SOURCE/p).is_file()]
    if '--test' in sys.argv:
        chosen = sys.argv[sys.argv.index('--test')+1]
        paths, guard = ([], [chosen]) if chosen.split('::')[0] in GUARD_FILES else ([chosen], [])
        cli_files = [chosen] if chosen.split('::')[0] in CLI_FILES else []
        if cli_files: paths, guard = [], []
    code, tests = run(paths) if paths else (0, {'paths': [], 'passed': 0, 'skipped': 0, 'not_applicable': []})
    assert code == 0, code
    cli_runs = {}
    if guard:
        ref._run_hook_guard = mainmod._run_hook_guard = native_hook_guard
        try:
            with native_graphify(cli_runs): code, guard_tests = run(guard, ignore=list(GUARD_NOT_APPLICABLE))
        finally:
            ref._run_hook_guard = mainmod._run_hook_guard = original_guard
        assert code == 0, code
        tests = {'helpers': tests, 'hook_guard': {**guard_tests, 'not_applicable_reasons': GUARD_NOT_APPLICABLE, 'native_hook_guard_runs': counts.get('hook-guard', 0), 'native_cli_runs': cli_runs}}
    if cli_files:
        command_line = {}; failed = []
        for path in cli_files:
            file = path.split('::')[0]
            runs = {}
            with native_graphify(runs):
                code, result = run([path], check='check_cli', differences={k: v for k, v in CLI_DIFFERENCES.items() if k.startswith(file + '::')},
                                   not_applicable_tests={k: v for k, v in CLI_NOT_APPLICABLE.items() if k.startswith(file + '::')})
            if code: failed.append(path)
            command_line[path] = {**result, 'native_cli_runs': runs}
            print(json.dumps({'command_line': path, 'exit': code, 'passed': result['passed'], 'native_cli_runs': runs}), flush=True)
        assert not failed, failed
        tests = {**tests, 'command_line': command_line}
    native.close()
    native.receipt('graphify/cli.py', [str(p.relative_to(ROOT)) for p in sorted((ROOT/'src').glob('cli*.rs'))]+['src/assets/cli/constants.json','src/assets/cli/help.txt'], checked=counts, upstream_tests=tests, qualification='Helper parity; complete executable workflows are recorded separately in native-workflows.json. The hook guard files (hook_guard) run every guard call, and every `python -m graphify` child, on the native executable.')
