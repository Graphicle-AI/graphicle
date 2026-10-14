"""Run unchanged pinned test files outside the embedding workspace's ignores.

A check that names its upstream_naming.py table gets the recorded name
translations applied to the temporary copy.

Two optional tables deselect single upstream tests by their exact file and
test name ('tests/<file>::<test>'; a parametrized test by its function name),
each with its reason, which the receipt lists:

- `differences`: the engine fails the test. A known difference, recorded
  rather than fixed.
- `not_applicable_tests`: the test checks something the engine does not have,
  such as a Python seam it patches or Graphify's own skill text.

Every entry must match a collected test.
"""
import sys
from pathlib import Path
import upstream_naming

def run(paths, *, ignore=(), check=None, differences=None, not_applicable_tests=None):
    import pytest
    differences = dict(differences or {})
    not_applicable_tests = dict(not_applicable_tests or {})
    tables = {**not_applicable_tests, **differences}
    assert len(tables) == len(differences) + len(not_applicable_tests), 'A test is in both tables'
    class Reports:
        passed = skipped = 0
        matched = set()
        kept_items = None
        def pytest_collection_modifyitems(self, config, items):
            if not tables: return
            kept, dropped = [], []
            for item in items:
                file = 'tests/' + Path(str(item.path)).name
                keys = [f'{file}::{item.name}', f'{file}::{getattr(item, "originalname", item.name)}']
                hit = next((key for key in keys if key in tables), None)
                (dropped if hit else kept).append(item)
                if hit: self.matched.add(hit)
            items[:] = kept
            self.kept_items = len(kept)
            config.hook.pytest_deselected(items=dropped)
        def pytest_runtest_logreport(self, report):
            if report.when == 'call' and report.passed: self.passed += 1
            if report.skipped: self.skipped += 1
    reports = Reports()
    # A check may run a second session (other substitutions) on a fresh copy:
    # the first copy's `tests` package must not shadow it.
    for name in [m for m in sys.modules if m == 'tests' or m.startswith('tests.')]:
        del sys.modules[name]
    with upstream_naming.isolated(paths, check) as (root, copies):
        args = ['-q', '-p', 'no:cacheprovider']
        if ignore: args += ['-k', ' and '.join('not '+name for name in ignore)]
        result = pytest.main(args+copies, plugins=[reports])
        # A file whose every test is in the tables runs none: pytest's "no tests
        # collected" is then the expected outcome, not a failure.
        if result == pytest.ExitCode.NO_TESTS_COLLECTED and reports.kept_items == 0 and reports.matched:
            result = pytest.ExitCode.OK
        stale = sorted(set(tables) - reports.matched)
        assert not stale, f'Deselected upstream tests that match no collected test: {stale}'
        record = {'paths': paths, 'passed': reports.passed, 'skipped': reports.skipped, 'not_applicable': list(ignore)}
        if not_applicable_tests: record['not_applicable_tests'] = not_applicable_tests
        if differences: record['known_differences'] = differences
        return result, record
