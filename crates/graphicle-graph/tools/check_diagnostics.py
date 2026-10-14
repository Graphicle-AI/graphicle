#!/usr/bin/env python3
"""Qualify native diagnostics without rewriting the retained Python tests."""
import contextlib, copy, inspect, io, json, random, sys, tempfile, types
from pathlib import Path
from parity import Native, SOURCE, ROOT
from naming import to_upstream
import graphify.diagnostics as reference
import graphify.security as security
import pytest

oracle = types.ModuleType('reference_diagnostics')
oracle.__package__ = 'graphify'
exec(compile((SOURCE/'graphify/diagnostics.py').read_text(), str(SOURCE/'graphify/diagnostics.py'), 'exec'), oracle.__dict__)
native = Native('diagnostics')
saved = {}
comparisons = {}
build_error_messages = []
def plain(value):
    if isinstance(value, Path): return str(value)
    if isinstance(value, (tuple, set)): return list(value)
    return value
def call(function, **kw):
    result = to_upstream(native.call('diagnostics', function=function, **{k: plain(v) for k,v in kw.items()}))
    if not result['ok']:
        message = result['error']
        if message.startswith('RuntimeError:'): raise RuntimeError(message)
        raise ValueError(message)
    result = result['result']
    for line in result.get('stdout', []): print(line)
    for line in result.get('stderr', []): print(line, file=sys.stderr)
    return result['value']
def wrapper(name):
    signature = inspect.signature(getattr(reference, name))
    def invoke(*args, **kw):
        bound = signature.bind(*args, **kw); bound.apply_defaults()
        params = dict(bound.arguments)
        if name == 'diagnose_file': params['max_graph_bytes'] = security._MAX_GRAPH_FILE_BYTES
        return call(name, **params)
    return invoke
class Reports:
    passed = 0; skipped = 0; deselected = []
    def pytest_collection_modifyitems(self, config, items):
        omitted = [i for i in items if '_cli_' in i.originalname]
        items[:] = [i for i in items if i not in omitted]
        self.deselected = [i.nodeid for i in omitted]
        config.hook.pytest_deselected(items=omitted)
    def pytest_runtest_logreport(self, report):
        if report.when == 'call' and report.passed: self.passed += 1
        if report.skipped: self.skipped += 1
reports = Reports()
def compare(function, **kw):
    with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
        expected = getattr(oracle, function)(**copy.deepcopy(kw))
        actual = call(function, **kw)
    comparisons[function] = comparisons.get(function, 0) + 1
    if function.startswith('diagnose_'):
        a, e = actual.get('post_build_error'), expected.get('post_build_error')
        if a and e:
            native_shape_check = a == 'TypeError: Edge must be an object' and e.startswith('AttributeError:')
            if a.split(':')[0] != e.split(':')[0] and not native_shape_check:
                native.fail(function=function, arguments={k:plain(v) for k,v in kw.items()}, expected=expected, actual=actual)
            if a != e: build_error_messages.append({'native':a, 'reference':e})
            actual = copy.deepcopy(actual); actual['post_build_error'] = e
    if actual != expected: native.fail(function=function, arguments={k:plain(v) for k,v in kw.items()}, expected=expected, actual=actual)
    return actual
try:
    names = ['diagnose_extraction','diagnose_file','format_diagnostic_json','format_diagnostic_report','scan_producer_suppression_sites']
    for name in names: saved[name] = getattr(reference,name); setattr(reference,name,wrapper(name))
    status = pytest.main(['-q','-p','no:cacheprovider',str(SOURCE/'tests/test_multigraph_diagnostics.py')], plugins=[reports])
    assert status == 0, status
    for name, value in saved.items(): setattr(reference,name,value)
    values = [None, False, True, 0, -42, 1.25, '', '日本語', ['a', None], {'z':1,'a':{'c':True,'b':'😀'}}]
    for value in values:
        compare('_safe_text', value=value)
        compare('_canonical_edge', edge=value)
        compare('_exact_signature', edge=value)
    rng = random.Random(73)
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary); source = root/'extract.py'
        source.write_text("seen_call_pairs: set[tuple[str, str]] = set()\n    seen_one = set()\nseen_blank: set[tuple[ ]] = set()\nother = set()\n", encoding='utf-8')
        compare('scan_producer_suppression_sites', path=source)
        compare('scan_producer_suppression_sites', path=root/'absent.py')
        for _ in range(200):
            line = rng.choice(['seen_pairs', '    seen_files', 'seen_', 'elsewhere', 'x.seen_bad']) + rng.choice([' = set()', ': set[tuple[str, str, str]] = set()', ': set[tuple[ ]] = set()', ': set[tuple[dict[str,int],str]] = set()'])
            compare('_tuple_arity_from_annotation', line=line)
        for iteration in range(300):
            nodes = [{'id':f'n{i}', 'label':f'Node{i}', 'file_type':'code', 'source_file':f'{i}.py'} for i in range(rng.randrange(1,12))]
            for n in nodes:
                if rng.random() < .15: n['verification'] = 'unverified'
            edges = []
            for _ in range(rng.randrange(40)):
                edge = {'source':rng.choice(nodes)['id'], 'target':rng.choice(nodes)['id'],
                        'relation':rng.choice(['calls','imports','references']), 'confidence':rng.choice(['EXTRACTED','INFERRED']),
                        'source_file':rng.choice(['a.py','b.py']), 'source_location':rng.choice(['L1','L2','']),
                        'context':rng.choice(['','uses','calls'])}
                if rng.random() < .12: edge['target'] = rng.choice(['ref_external','missing',''])
                if rng.random() < .1: edge['from'] = edge.pop('source')
                edges.append(edge)
                if rng.random() < .15: edges.append(copy.deepcopy(edge))
            if iteration % 3 == 0: edges.extend([None, [], {}, {'source':'n0','target':{'id':'unhashable'}}])
            extraction = {'nodes':nodes, rng.choice(['edges','links']):edges}
            kw = dict(directed=rng.choice([False,True]), extract_path=source, max_examples=rng.randrange(7))
            summary = compare('diagnose_extraction', extraction=extraction, **kw)
            compare('format_diagnostic_json', summary=summary)
            compare('format_diagnostic_report', summary=summary)
            if iteration % 15 == 0:
                path = root/'graph.json'; extraction['directed'] = rng.choice([True,False,'true',None])
                path.write_text(json.dumps(extraction), encoding='utf-8'); before = path.read_bytes()
                compare('diagnose_file', path=path, directed=None, extract_path=source)
                assert path.read_bytes() == before
        # Rust scans expose the compiled producer snapshot, not upstream Python locations.
        rust = root/'producer.rs'
        rust.write_text('let mut seen_pairs: HashSet<(String, String)> = HashSet::new();\n    let seen_ids = HashSet::new();\nlet unrelated = 0;\n')
        scan = call('scan_producer_suppression_sites', path=rust)
        assert scan['total_sites'] == 2 and scan['sites'][0]['tuple_arity'] == 2, scan
        compiled = call('diagnose_extraction', extraction={'nodes':[],'edges':[]})['producer_suppression']
        assert compiled['path'] == 'native:compiled-producers' and compiled['total_sites'] > 0, compiled
        for site in compiled['sites']:
            line = (ROOT/site['source_file']).read_text().splitlines()[site['line']-1]
            assert site['name'] in line and site['sample'] == line.strip()[:120], site
        path = root/'graph.json'; path.write_text('[]')
        try: call('diagnose_file', path=path)
        except ValueError as error: assert 'JSON object' in str(error)
        else: native.fail(reason='Accepted non-object diagnostic input')
        path.write_text('{broken')
        try: call('diagnose_file', path=path)
        except RuntimeError as error: assert 're-run' in str(error)
        else: native.fail(reason='Accepted malformed JSON')
    native.receipt('graphify/diagnostics.py', ['src/diagnostics.rs','src/diagnostics_report.rs','build.rs'],
        upstream_tests={'passed':reports.passed,'skipped':reports.skipped,'deselected':reports.deselected},
        comparisons=comparisons, build_error_message_differences=sorted({json.dumps(e,sort_keys=True) for e in build_error_messages}),
        adaptations=['Default producer suppression scans the native producer source snapshot embedded at compile time; Rust sites include relative source_file. An explicit Python path retains the upstream scan behavior.',
                     'Build failures preserve native builder messages. Invalid edge shapes fail with an explicit TypeError instead of the Python implementation\'s incidental AttributeError; exact Python exception wording is not a runtime dependency.',
                     'CLI option parsing belongs to the pending native application entry point and is not claimed by these library tests.'])
finally:
    for name,value in saved.items(): setattr(reference,name,value)
    native.close()
