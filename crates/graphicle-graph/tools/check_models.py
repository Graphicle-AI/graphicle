#!/usr/bin/env python3
"""Compare native extraction records with the pinned Python dataclasses."""
import dataclasses
from pathlib import Path
from parity import Native
import graphify.extract as extract
from graphify.extractors import models

native = Native('models')
comparisons = 0

def plain(value):
    if dataclasses.is_dataclass(value):
        return {field.name: plain(getattr(value, field.name)) for field in dataclasses.fields(value)}
    if isinstance(value, Path):
        return str(value)
    if isinstance(value, (set, frozenset)):
        return sorted(value)
    if isinstance(value, (tuple, list)):
        return [plain(v) for v in value]
    if isinstance(value, dict):
        return {k: plain(v) for k, v in value.items()}
    if callable(value):
        return None
    return value

def compare(name, supplied, expected):
    global comparisons
    actual = native.result('models', name=name, value=plain(supplied))
    comparisons += 1
    if actual != plain(expected):
        native.fail(name=name, supplied=plain(supplied), expected=plain(expected), actual=actual)

try:
    compare('LanguageConfig', {'ts_module': 'custom'}, models.LanguageConfig('custom'))
    configs = [v for v in vars(extract).values() if isinstance(v, models.LanguageConfig)]
    for value in configs:
        compare('LanguageConfig', value, value)
    path = Path('workspace/日本語 file.ts')
    target = Path('/library/δ/index.ts')
    records = [
        models._SymbolDeclarationFact(path, 'Hello', 1),
        models._SymbolImportFact(path, 'Alias', target, 'Original', 18),
        models._SymbolAliasFact(path, 'Renamed', 'Original', 21),
        models._SymbolExportFact(path, 'default', 22),
        models._SymbolExportFact(path, 'Other', 23, 'Here', target, 'There', True),
        models._StarExportFact(path, target, 24),
        models._StarExportFact(path, target, 25, True),
        models._NamespaceExportFact(path, 'Namespace', target, 26),
        models._NamespaceExportFact(path, 'Types', target, 27, True),
        models._SymbolUseFact(path, 'source-id', 'Alias', 'calls', 'Alias()', 28),
    ]
    for value in records:
        supplied = plain(value)
        # Omit default-valued fields to exercise the native constructors too.
        for field in dataclasses.fields(value):
            if field.default is not dataclasses.MISSING and getattr(value, field.name) == field.default:
                supplied.pop(field.name)
        compare(type(value).__name__, supplied, value)
    facts = models._SymbolResolutionFacts(
        declarations=[records[0]], imports=[records[1]], aliases=[records[2]],
        exports=records[3:5], star_exports=records[5:7], namespace_exports=records[7:9],
        uses=[records[9]], module_imports=[(path, target, 30, 'module')])
    compare('_SymbolResolutionFacts', facts, facts)
    compare('_SymbolResolutionFacts', {}, models._SymbolResolutionFacts())
    assert sorted(native.result('models', name='suffixes')) == sorted(models._JS_CACHE_BYPASS_SUFFIXES)
    assert native.result('models', name='workspace_cache', clear=True) == {}
    expected = {'project': {'@workspace/library': str(target)}}
    assert native.result('models', name='workspace_cache', key='project', value=expected['project']) == expected
    assert native.result('models', name='workspace_cache') == expected
    assert native.result('models', name='workspace_cache', clear=True) == {}
    for field in ('import_handler', 'resolve_function_name_fn', 'sanitize_symbol_name_fn', 'extra_walk_fn'):
        response = native.call('models', name='LanguageConfig', value={'ts_module':'custom', field:'not-a-function'})
        assert not response['ok'] and 'native callback API' in response['error']
    native.receipt('graphify/extractors/models.py', ['src/extractors/models.rs'],
        record_comparisons=comparisons, original_language_configurations=len(configs),
        callback_unit_test='extractors::models::tests::callbacks_receive_real_nodes_and_mutable_output',
        adaptations=[
            'Python frozensets and tuples map to native BTreeSets and vectors; serialized sets are compared without order.',
            'Callable fields use typed native closures. The JSON boundary rejects fake serialized callbacks; a Rust unit exercises all four callbacks on real tree-sitter nodes.',
            'Frozen fact fields have read-only public accessors. The shared workspace cache persists behind a mutex.',
            'The pinned generic engine never invokes extra_walk_fn. Its callback slot is retained, without claiming an upstream traversal invocation that does not exist.'
        ])
finally:
    native.close()
