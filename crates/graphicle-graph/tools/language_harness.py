"""Shared differential harness for the single-file extractors added upstream
after 0.9.65 (COBOL, R, Solidity, VB.NET, Erlang).

Every per-file extraction the unchanged upstream tests perform is answered by
Rust and compared with the pinned Python function; the language's corpus
resolver is wrapped the same way. Generated sources, the upstream fixture and
multi-file corpora are then compared through the complete `extract` pipeline.
"""
import copy
import dataclasses
import importlib
import json
import shutil
import tempfile
from contextlib import ExitStack
from pathlib import Path
from unittest.mock import patch

import pytest
from parity import Native, SOURCE
import graphify.extract as facade
import graphify.resolver_registry as registry


def plain(value):
    return json.loads(json.dumps(value))


def canonical(value):
    value = plain(value)
    if isinstance(value, dict):
        for key in ('nodes', 'edges', 'raw_calls'):
            if isinstance(value.get(key), list):
                value[key].sort(key=lambda item: json.dumps(item, sort_keys=True))
    return value


class Harness:
    def __init__(self, language, module, *, resolver=None):
        self.language = language
        self.native = Native(language)
        self.reference = importlib.import_module(f'graphify.extractors.{module}')
        self.original = getattr(self.reference, f'extract_{language}')
        self.resolver_name = resolver
        self.counts = {'files': 0, 'resolver': 0, 'corpora': 0}

    def extract(self, path):
        # The Python extractor may raise (upstream's `_safe_extract` turns that
        # into an error record); Rust returns the same `Type: message` error.
        try:
            expected, raised = plain(self.original(path)), None
        except Exception as error:  # noqa: BLE001 - mirrored, then re-raised
            expected, raised = None, error
        response = self.native.call(f'extract.{self.language}', path=str(path))
        self.counts['files'] += 1
        source = Path(path).read_bytes().hex() if Path(path).is_file() else None
        if raised is not None:
            wanted = f'{type(raised).__name__}: {raised}'
            if response.get('ok') or response.get('error') != wanted:
                self.native.fail(path=str(path), source=source, expected_error=wanted, actual=response)
            raise raised
        actual = response.get('result') if response.get('ok') else response
        if expected != actual:
            self.native.fail(path=str(path), source=source, expected=expected, actual=actual)
        return actual

    def resolve(self, original):
        def wrapped(per_file, all_nodes, all_edges):
            before = plain({'per_file': per_file, 'nodes': all_nodes, 'edges': all_edges})
            expected_edges = copy.deepcopy(all_edges)
            original(copy.deepcopy(per_file), copy.deepcopy(all_nodes), expected_edges)
            actual = self.native.result(f'{self.language}.resolve', **before)
            self.counts['resolver'] += 1
            if plain(expected_edges) != actual['edges']:
                self.native.fail(resolver=self.resolver_name, request=before,
                                 expected=plain(expected_edges), actual=actual['edges'])
            # Apply the (identical) mutation to the caller's own objects.
            original(per_file, all_nodes, all_edges)
        return wrapped

    def patched(self):
        stack = ExitStack()
        name = f'extract_{self.language}'
        stack.enter_context(patch.object(self.reference, name, self.extract))
        stack.enter_context(patch.object(facade, name, self.extract))
        for table in (facade._DISPATCH, facade._SHEBANG_DISPATCH):
            for key, value in list(table.items()):
                if value is self.original:
                    stack.enter_context(patch.dict(table, {key: self.extract}))
        if self.resolver_name:
            index = next(i for i, r in enumerate(registry._REGISTRY) if r.name == self.resolver_name)
            entry = registry._REGISTRY[index]
            registry._REGISTRY[index] = dataclasses.replace(entry, resolve=self.resolve(entry.resolve))
            stack.callback(registry._REGISTRY.__setitem__, index, entry)
        return stack

    def upstream(self, test_file, deselect=()):
        """Run the unchanged upstream test file against Rust answers."""
        class Reports:
            passed = 0
            skipped = 0
            deselected = []

            def pytest_collection_modifyitems(self, config, items):
                omitted = [i for i in items if i.originalname in deselect]
                items[:] = [i for i in items if i.originalname not in deselect]
                self.deselected = [i.nodeid for i in omitted]
                config.hook.pytest_deselected(items=omitted)

            def pytest_runtest_logreport(self, report):
                if report.when == 'call' and report.passed:
                    self.passed += 1
                if report.skipped:
                    self.skipped += 1
        reports = Reports()
        with tempfile.TemporaryDirectory(prefix=f'graphify-{self.language}-upstream-') as tmp:
            # Outside the enclosing checkout, whose .gitignore excludes the
            # downloaded reference tree, so fixture discovery matches upstream.
            isolated = Path(tmp)
            shutil.copytree(SOURCE / 'tests', isolated / 'tests')
            with self.patched():
                status = pytest.main(['-q', '-p', 'no:cacheprovider', str(isolated / test_file)],
                                     plugins=[reports])
        assert status == 0, f'Original upstream tests failed: {status}'
        return {'paths': [test_file], 'passed': reports.passed, 'skipped': reports.skipped,
                'deselected': reports.deselected}

    def corpus(self, files):
        """Compare the complete extraction pipeline on one multi-file corpus."""
        with tempfile.TemporaryDirectory(prefix=f'graphify-{self.language}-corpus-') as tmp:
            root = Path(tmp).resolve()
            paths = []
            for relative, text in files.items():
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(text if isinstance(text, bytes) else text.encode())
                paths.append(path)
            with tempfile.TemporaryDirectory() as python_cache, tempfile.TemporaryDirectory() as rust_cache:
                expected = facade.extract(paths, cache_root=Path(python_cache), root=root)
                self.native.use_working_directory(Path.cwd())
                actual = self.native.result('extract', paths=[str(p) for p in paths],
                                            root=str(root), cache_root=rust_cache)
        self.counts['corpora'] += 1
        if canonical(expected) != canonical(actual):
            self.native.fail(corpus=files if all(isinstance(v, str) for v in files.values()) else None,
                             expected=canonical(expected), actual=canonical(actual))

    def checked(self, path):
        """`extract` for generated inputs: a mirrored extractor exception is an
        expected outcome once both sides agreed on it."""
        try:
            self.extract(path)
        except AssertionError:
            raise
        except Exception:  # noqa: BLE001 - compared inside extract()
            pass

    def files(self, suffix, sources, names=()):
        """Compare single-file extraction over generated and edge-case inputs."""
        with tempfile.TemporaryDirectory(prefix=f'graphify-{self.language}-') as tmp:
            root = Path(tmp)
            path = root / f'sample{suffix}'
            for text in sources:
                if isinstance(text, bytes):
                    path.write_bytes(text)
                else:
                    path.write_text(text, encoding='utf-8')
                self.checked(path)
            sample = next((t for t in reversed(sources) if isinstance(t, str) and t), '')
            for name in names:
                named = root / name
                named.parent.mkdir(parents=True, exist_ok=True)
                named.write_text(sample, encoding='utf-8')
                self.checked(named)
            self.checked(root / f'missing{suffix}')
            self.checked(root)

    def close(self):
        self.native.close()

    def receipt(self, source, rust_files, **evidence):
        self.native.receipt(source, rust_files, checked=self.counts, **evidence)
