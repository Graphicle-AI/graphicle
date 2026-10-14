"""Compare extraction against pinned source with explicit reviewed adaptations."""
import copy
from contextlib import chdir
import functools
import importlib
import inspect
import json
import multiprocessing
import shutil
import tempfile
import sys
from pathlib import Path
from parity import LEDGER, Native, ROOT, SOURCE
from isolated_upstream import run
import naming
import upstream_fixes

# The extraction facade binds extractor functions at import time, so the
# reviewed post-pin fixes (migration/upstream-review/adaptations.json) are
# applied to the pinned extractor modules in memory before it is imported.
# The pinned checkout itself is never edited.
for _relative in upstream_fixes.adapted_modules():
    if _relative in ('graphify/build.py', 'graphify/llm.py', 'graphify/extract.py'):continue
    _module=importlib.import_module(_relative[:-3].replace('/','.'))
    exec(compile(upstream_fixes.oracle_source(_relative),f'<pinned {_relative} with reviewed fixes>','exec'),_module.__dict__)
import graphify.extract as ref
exec(compile(upstream_fixes.oracle_source('graphify/extract.py'),
             '<pinned graphify/extract.py with reviewed Astro fix>', 'exec'), ref.__dict__)
native=Native('extract')
native_cache=native.result('cache',function='snapshot',arguments={})['value']
# The AST directory also names the grammar set (v{version}-s{schema}-g{grammars}, #4236).
native_grammars=native.result('cache',function='_grammar_fingerprint',arguments={})['value']
native_ast_namespace=f"v{native_cache['extractor_version']}-s{native_cache['schema']}-g{native_grammars}"
original={n:v for n,v in vars(ref).items() if callable(v)}
counts={};failures=[];in_reference=False

def serial(v):
    if isinstance(v,Path):return str(v)
    if isinstance(v,dict):return {str(k):serial(x) for k,x in v.items()}
    if isinstance(v,(tuple,list,set,frozenset)):return [serial(x) for x in v]
    return v

def canonical(v):
    v=copy.deepcopy(serial(v))
    if isinstance(v,dict):
        for k in ('nodes','edges'):
            if isinstance(v.get(k),list):v[k].sort(key=lambda x:json.dumps(x,sort_keys=True))
    return v

def compare(name,operation,expected,request):
    native.use_working_directory(Path.cwd())
    # Fixture ignore files are read by both sides, each under its own name.
    near=[parent for p in [request.get('path'),*request.get('paths',[])] if p for parent in Path(p).resolve().parents]
    trees=[request[key] for key in ('root','scan_root') if request.get(key)]
    # collect_files reads the ignore file of the folder it walks (0.9.88
    # test_unanchored_pattern_reaches_every_consumer_of_the_engine). Only that
    # folder is searched: a recursive search would show up in the tests that
    # count os.scandir calls (test_collect_files_walks_each_directory_once).
    if request.get('action')=='collect' and request.get('path') and Path(request['path']).is_dir():near.append(Path(request['path']).resolve())
    with naming.native_inputs(*near,*trees,recursive=trees):actual=native.result(operation,**serial(request))
    counts[name]=counts.get(name,0)+1
    wanted=canonical(expected);got=canonical(actual)
    # XML implementations report their own parse diagnostics; validity, refusal
    # of entities and size limits are independently checked by upstream tests.
    if isinstance(wanted,dict) and isinstance(got,dict) and 'error' in wanted and 'error' in got:
        if 'XML parse error' in str(wanted['error']) and 'XML parse error' in str(got['error']):wanted['error']=got['error']='XML parse error'
    if wanted!=got:
        files={}
        for p in [request.get('path'),*request.get('paths',[])]:
            if p and Path(p).is_file():files[str(p)]=Path(p).read_text(errors='replace')
        failure={'name':name,'request':serial(request),'files':files,'expected':serial(expected),'actual':actual}
        failures.append(failure)
        (ROOT/'migration/receipts/extract-differences.json').write_text(json.dumps(failures,indent=2)+'\n')
        raise AssertionError('Native extraction differs: '+name)
    return actual

def expected_call(name,*args,**kw):
    global in_reference
    old=in_reference;in_reference=True
    try:return original[name](*args,**kw)
    finally:in_reference=old

def install(name,impl):
    @functools.wraps(original[name])
    def wrapper(*args,**kw):
        if in_reference or multiprocessing.current_process().name != 'MainProcess':return original[name](*args,**kw)
        return impl(*args,**kw)
    setattr(ref,name,wrapper)
    for table in (ref._DISPATCH,ref._SHEBANG_DISPATCH):
        for key,value in list(table.items()):
            if value is original[name]:table[key]=wrapper

languages=['python','js','svelte','astro','vue','ejs','java','groovy','c','cpp','ruby','csharp','kotlin','scala','php','lua','swift']
for language in languages:
    name='extract_'+language
    def impl(path,_language=language,_name=name,**kw):
        # extract_python(path, root=...) since #3729: the scan root anchors imports.
        request={'path':path,'language':_language}
        if kw.get('root') is not None:request['scan_root']=kw['root']
        return compare(_name,'extract.language',expected_call(_name,path,**kw),request)
    install(name,impl)
for language in ['lazarus_package','slnx','csproj','xaml']:
    name='extract_'+language
    def impl(path,_language=language,_name=name):
        return compare(_name,'extract.project',expected_call(_name,path),{'path':path,'language':_language,'root':ref._XAML_ACTIVE_EXTRACT_ROOT})
    install(name,impl)

def extraction(paths,cache_root=None,**kw):
    paths=list(paths)
    # Each implementation starts with the same cache contents. Native cache
    # cleanup must not remove the oracle's independently versioned namespace.
    inferred=Path('.')
    if len(paths)==1:inferred=paths[0].parent
    elif paths:
        parts=list(zip(*(p.parts for p in paths)))
        common=[]
        for group in parts:
            if len(set(group))!=1:break
            common.append(group[0])
        inferred=Path(*common)
    root=Path(kw.get('root') or cache_root or inferred).resolve()
    location=Path(cache_root or '.').resolve()
    with tempfile.TemporaryDirectory(prefix='graphify-native-extract-cache-') as tmp:
        source=location/'graphify-out/cache/ast'
        target=Path(tmp)/'graphify-out/cache/ast'/native_ast_namespace
        # Only the reference's current namespace is read; entries left under
        # another version, schema or grammar set stay stale on both sides.
        from graphify import cache as ref_cache
        current=source/f"v{ref_cache._EXTRACTOR_VERSION}-s{ref_cache._AST_CACHE_SCHEMA}-g{ref_cache._grammar_fingerprint()}"
        if current.is_dir():
            shutil.copytree(current,target)
            # Cached entries carry the portable root marker under each side's name.
            for entry in target.rglob('*.json'):
                text=entry.read_text(encoding='utf-8')
                if naming.UPSTREAM_ROOT_MARKER in text:entry.write_text(naming.native_markers(text),encoding='utf-8')
        expected=expected_call('extract',paths,cache_root,**kw)
        return compare('extract','extract',expected,{'paths':paths,**kw,'root':root,'cache_root':tmp})
install('extract',extraction)

def collect(target,*,follow_symlinks=False,root=None):
    actual=compare('collect_files','extract',expected_call('collect_files',target,follow_symlinks=follow_symlinks,root=root),{'action':'collect','path':target,'follow_symlinks':follow_symlinks,'root':root})
    return [Path(p) for p in actual]
install('collect_files',collect)

names=['extract_'+lang for lang in languages]+['extract_xaml','extract_csproj','extract_slnx','extract_lazarus_package','collect_files']
paths=[str(p.relative_to(SOURCE)) for p in sorted((SOURCE/'tests').glob('test_*.py')) if any(n in p.read_text() for n in names)]
# The Python CLI re-execs itself to pin PYTHONHASHSEED (#3743/#3780/#3816);
# those cases spawn an interpreter and never reach an extractor. Native
# binaries have no per-process hash randomization to pin.
paths=[p for p in paths if p!='tests/test_pin_hash_seed.py']
# Java calls to inherited methods and through super (0.9.73) resolve in the
# full extract() pipeline, which these tests drive without naming an extractor.
paths+=[p for p in ['tests/test_java_member_calls.py'] if p not in paths and (SOURCE/p).is_file()]
# 0.9.78-0.9.88 contracts that drive extract() without naming an extractor:
# typed receivers, Self and forward references, C#/Java/PHP/Swift/TS resolution,
# C includes, source decoding, the path memo and the incomplete-coverage note.
EXTRACT_0988=[
    'tests/test_python_typed_receiver_calls.py','tests/test_python_self_type_ref.py','tests/test_python_forward_ref_file_id.py',
    'tests/test_python_nested_class_identity.py','tests/test_python_reexport_type_binding.py','tests/test_qualified_annotation_stub_bind.py',
    'tests/test_python_decorators.py','tests/test_csharp_member_chain_calls.py','tests/test_csharp_var_call_return_type.py',
    'tests/test_csharp_verbatim_interpolation.py','tests/test_csharp_enum_members.py','tests/test_pr10_csharp_overrides.py',
    'tests/test_java_anonymous_class.py','tests/test_php_static_call_resolution.py','tests/test_php_object_creation.py',
    'tests/test_swift_self_type_ref.py','tests/test_ts_export_type_star.py','tests/test_ts_receiver_member_calls.py',
    'tests/test_loose_sibling_import_resolution.py','tests/test_c_include_resolution.py','tests/test_source_file_encoding.py',
    'tests/test_extract_path_memo.py','tests/test_resolve_memoization.py','tests/test_partial_extraction_warning.py',
    'tests/test_no_cluster_run_only_keys.py::test_run_only_keys_are_the_ones_extract_returns',
]
paths+=[p for p in EXTRACT_0988 if p not in paths and (SOURCE/p.split('::')[0]).is_file()]
# Upstream files that drive extract() without naming an extractor, first run
# here on 14 October 2026 (all unchanged since 0.9.77 but the 0.9.88 skill
# Part A file): language features, cross-file and cross-language resolution,
# node IDs and the extraction cache.
EXTRACT_UNCHANGED=[
    'tests/test_builtin_global_type_refs.py','tests/test_case_sensitive_resolution.py',
    'tests/test_cpp_method_declarations.py','tests/test_cross_language_call_resolution.py',
    'tests/test_csharp_call_site_generic_args.py','tests/test_csharp_field_generic_args.py',
    'tests/test_csharp_generic_callsites.py','tests/test_csharp_member_nodes.py',
    'tests/test_csharp_tuple_type_refs.py','tests/test_definition_file_portability.py',
    'tests/test_duplicate_annotation_edges.py','tests/test_elixir_import_resolution.py',
    'tests/test_elixir_unqualified_call_scope.py','tests/test_extract_cache_location.py',
    'tests/test_file_node_id_spec.py','tests/test_import_self_loops.py','tests/test_incremental.py',
    'tests/test_inherited_field_receivers.py','tests/test_issue_3405_python_resolution.py',
    'tests/test_java_enum_members.py','tests/test_js_callback_calls.py','tests/test_js_destructured_export.py',
    'tests/test_js_exported_fn_call_resolution.py','tests/test_js_module_level_anonymous_calls.py',
    'tests/test_js_this_member_expressions.py','tests/test_jsx_component_usage_calls.py',
    'tests/test_kotlin_grammar.py','tests/test_kotlin_object_literal.py','tests/test_lua_import.py',
    'tests/test_lua_method_calls.py','tests/test_node_id_canonical.py','tests/test_objc_field_table_remap.py',
    'tests/test_owned_type_sources.py','tests/test_package_json_subpath_imports.py',
    'tests/test_phantom_cross_package_call.py','tests/test_ruby_bare_self_send_calls.py',
    'tests/test_scala_abstract_methods.py','tests/test_skill_part_a_explicit_root.py',
    'tests/test_sourceless_call_targets.py','tests/test_swift_cross_file_calls.py',
    'tests/test_swift_import_resolution.py','tests/test_ts_abstract_methods.py','tests/test_ts_decorators.py',
    'tests/test_ts_generators.py','tests/test_ts_import_require.py','tests/test_ts_import_type_arguments.py',
    'tests/test_ts_inheritance.py','tests/test_ts_namespace.py','tests/test_ts_parse_warning.py',
    'tests/test_typescript_enum_members.py','tests/test_zero_node_no_cache.py',
]
paths+=[p for p in EXTRACT_UNCHANGED if p not in paths and (SOURCE/p).is_file()]
# Upstream tests the engine fails, with the reason: known differences, recorded
# and not fixed. Each is deselected by its exact file and test name and listed
# in the receipt (isolated_upstream.run).
KNOWN_DIFFERENCES={
    'tests/test_typescript_enum_members.py::test_case_only_sibling_members_do_not_duplicate_an_edge':
        'Two TypeScript enum members whose names differ only in case (`Value`, `value`) share one node id. '
        'The reference keeps one `case_of` edge to it; the engine also emits a second one from the later member\'s line.',
}
# Upstream tests of those files that check something the engine does not have.
NOT_APPLICABLE_TESTS={
    'tests/test_zero_node_no_cache.py::test_zero_node_result_not_cached_then_self_heals':
        'Forces a zero-node result by patching the reference\'s private `_safe_extract_with_xaml_root`; '
        'the engine has no Python seam to patch, so its extraction is unchanged.',
    'tests/test_skill_part_a_explicit_root.py::test_split_hosts_are_discovered':
        'Reads the skill files that Graphify\'s skill generator (`tools/skillgen`) renders; the engine ships its own skill files.',
    'tests/test_skill_part_a_explicit_root.py::test_part_a_passes_an_explicit_root':
        'Reads the skill files that Graphify\'s skill generator (`tools/skillgen`) renders; the engine ships its own skill files.',
}
# Files of the ported releases this check does not run, with the reason (receipt).
NOT_RUN={'tests/test_js_walk_order.py':'Tests the reference\'s private _walk_js_tree TreeCursor walk (0.9.80, a speed rewrite not ported): '
         'no extraction runs, so nothing native is reached. The JS/TS fact order that walk feeds is compared by check_resolution (js_facts) and by every extract() comparison here.'}
if '--test' in sys.argv:paths=[sys.argv[sys.argv.index('--test')+1]]
if '--layers' in sys.argv:setattr(ref,'extract',original['extract'])
if __name__ == '__main__':
    # Unspecified cache roots are relative to cwd. Other concurrent oracles
    # write/clean their caches, so copying the shared crate cache can race with
    # an atomic write or deletion. Both implementations keep their existing
    # cache contracts inside this check's own working directory.
    with tempfile.TemporaryDirectory(prefix='graphify-extract-oracle-') as workspace, chdir(workspace):
        for attribute in [
            '', 'type', 'type=""', 'type=module', 'type="module"', "type='module'",
            'TYPE="MODULE"', 'type = "text/javascript"', "type='application/javascript'",
            'data-type="application/json"', "data-note='type=application/json'",
            'type="module" type="application/json"', 'type="application/json" type="module"',
            'type="application/ld+json"', "type='importmap'", 'type=module_x',
        ]:
            for prefix in ['', '---\nconst first = 1;\n---\n', '<p>\u00e9</p>\r\n']:
                # A trailing line comment must not run into the next script (#4072).
                for body in ['function live() {}\n', 'live(); // trailing note', 'first() /* open']:
                    source = prefix + f'<script {attribute}>{body}</script>\n<p>\u2603</p><script>second()</script>'
                    compare('astro_mask_reviewed', 'extract.language', ref._astro_mask_non_script(source),
                            {'action': 'astro_mask', 'source': source})
        code,tests=run(paths,ignore=['test_extract_generic_surfaces_tree_sitter_version_mismatch_hint',
            'test_extract_twice_failing_file_carries_error_marker',
            'test_extract_legitimately_empty_result_keeps_no_error_marker',
            'test_extract_warns_when_sql_extra_missing','test_extract_warns_sql_grammar_failed_to_load',
            'test_failed_extra_is_retried_and_recovers','test_permanent_failure_does_not_wedge',
            'test_poisoned_manifest_is_healed','test_extract_genuinely_empty_json_still_failed',
            # Asserts the pre-#3270 solution-folder node; the adopted fix is covered by check_sln.py.
            'test_sln_solution_folder_ids_are_relative'],
            differences={k:v for k,v in KNOWN_DIFFERENCES.items() if k.split('::')[0] in {p.split('::')[0] for p in paths}},
            not_applicable_tests={k:v for k,v in NOT_APPLICABLE_TESTS.items() if k.split('::')[0] in {p.split('::')[0] for p in paths}})
        native.close()
    (ROOT/'migration/receipts/extract-differences.json').write_text(json.dumps(failures,indent=2)+'\n')
    assert code==0 and not failures,'Native extraction comparison failures'
    native.receipt('graphify/extract.py',[str(p.relative_to(ROOT))for p in sorted((ROOT/'src').glob('extract*.rs'))]+['src/assets/extract/constants.json'],checked=counts,upstream_tests=tests,upstream_tests_not_run=NOT_RUN,interface_differences=['Native worker threads replace Python process pools. XML parser diagnostics use native error messages.'],adaptations=['Reviewed 2026-10-02 Astro type parsing: preserve quoted JavaScript types; only the first actual type attribute controls classification.', 'Reviewed 2026-10-08 cache portability: source paths and IDs relocate together for walked directory aliases and final file symlinks; the independent cache.py adaptation is installed before the extraction facade. See migration/upstream-review/adaptations.json.'])
