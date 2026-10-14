"""Development-only harness: run pinned Python contracts against native Rust.

The shipped Rust library/binary does not import or invoke this harness.
"""
import hashlib
import json
import os
import platform
import subprocess
import sys
import time
from pathlib import Path

sys.dont_write_bytecode = True
# The name translation table lives beside this file; an edition's harness loads
# this module from its own tools/ folder, whose modules keep precedence.
if str(Path(__file__).resolve().parent) not in sys.path:
    sys.path.append(str(Path(__file__).resolve().parent))
import naming

ROOT = Path(__file__).resolve().parents[1]
LEDGER = json.loads((ROOT / "migration/FILES.json").read_text())
SOURCE = Path(os.environ.get("GRAPHICLE_ORACLE_SOURCE") or LEDGER["source_root"])
if not SOURCE.is_absolute():
    SOURCE = ROOT / SOURCE
SOURCE = SOURCE.resolve()
BINARY = Path(os.environ.get("GRAPHICLE_GRAPH_BINARY", ROOT / "target/release" / naming.NATIVE_BIN)).resolve()
sys.path.insert(0, str(SOURCE))


def validate_upstream(info, ledger=LEDGER):
    """A renamed release cannot qualify against an older local inventory.

    The native `info` operation reports the pinned release it ports under
    `based_on` (attribution metadata).
    """
    based_on = info.get('based_on') if isinstance(info.get('based_on'), dict) else {}
    for field in ('upstream_commit', 'upstream_version'):
        if based_on.get(field) != ledger[field]:
            raise ValueError(f'Native {field} differs from migration/FILES.json; '
                             'refresh tools/inventory.py and rebuild before qualification')
    count = sum(row['category'] == 'application_python' for row in ledger['files'])
    if based_on.get('application_modules') != count:
        raise ValueError('Native application module count differs from the pinned inventory')


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


class Native:
    """One long-lived native JSON Lines process.

    layout='upstream' (the default) runs the engine with the reference's output
    folder name, so locations the native chooses match unchanged upstream
    tests; layout='native' keeps the engine's own defaults. Either way every
    GRAPHIFY_<X> setting is copied to GRAPHICLE_<X> (tools/naming.py).
    """
    def __init__(self, name, cwd=None, layout='upstream'):
        if layout not in ('upstream', 'native'):
            raise ValueError(f'unknown layout {layout!r}')
        self.name = name
        self.layout = layout
        self.started = time.monotonic()
        self.calls = 0
        self.failures = 0
        self.binary_sha256 = sha(BINARY)
        from qualification import harness_sources
        self.harness_files = harness_sources()
        self.qualification = os.environ.get('GRAPHICLE_QUALIFICATION_MANIFEST')
        if self.qualification:
            from qualification import validate
            validate(self.qualification, BINARY)
        self.cwd = os.path.realpath(cwd or os.getcwd())
        self.process = subprocess.Popen(
            [str(BINARY)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, cwd=cwd,
            env=self.environment(),
        )
        try:
            validate_upstream(self.result('info'))
        except BaseException:
            self.close()
            raise

    def environment(self):
        """The native process environment for the current os.environ."""
        return naming.native_env(os.environ, self.layout)

    def use_working_directory(self, cwd):
        """Restart a stateless test process when an original fixture changes cwd.

        This deliberately does not change the native library's process directory
        through its production API, or change the Python test process's cwd.
        Callers must not use it to test stateful caches or registry persistence.
        """
        resolved = os.path.realpath(cwd)
        if resolved != self.cwd:
            self.close()
            assert sha(BINARY) == self.binary_sha256, "Native binary changed during comparison"
            self.process = subprocess.Popen(
                [str(BINARY)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                text=True, cwd=resolved, env=self.environment(),
            )
            self.cwd = resolved

    def call(self, operation, **kwargs):
        self.calls += 1
        if 'env' in kwargs:
            kwargs['env'] = naming.native_keys(kwargs['env'])
        request = {"operation": operation, **kwargs}
        self.process.stdin.write(json.dumps(request, ensure_ascii=True, allow_nan=False) + "\n")
        self.process.stdin.flush()
        response = self.process.stdout.readline()
        if not response:
            raise RuntimeError(f"Native process exited during {operation}")
        return json.loads(response)

    def result(self, operation, **kwargs):
        response = self.call(operation, **kwargs)
        if not response.get("ok"):
            raise ValueError(response.get("error", response))
        return response["result"]

    def fail(self, **evidence):
        self.failures += 1
        (ROOT / f"migration/receipts/{self.name}-failure.json").write_text(
            json.dumps(evidence, ensure_ascii=True, indent=2) + "\n"
        )
        raise AssertionError(f"Native mismatch: migration/receipts/{self.name}-failure.json")

    def close(self):
        self.process.stdin.close()
        self.process.wait(timeout=10)
        if self.process.returncode:
            raise RuntimeError(f"Native process exit {self.process.returncode}")

    def receipt(self, source, rust_files, naming_extra=None, **evidence):
        assert not self.failures, f"{self.failures} comparison failures were caught by a caller; no passing receipt may be issued"
        assert sha(BINARY) == self.binary_sha256, "Native binary changed during comparison"
        from qualification import harness_sources
        assert harness_sources() == self.harness_files, 'Qualification tools changed during comparison'
        evidence['harness_files'] = self.harness_files
        import upstream_naming
        if upstream_naming.applied():
            naming_extra = {**(naming_extra or {}), 'upstream_test_replacements': upstream_naming.applied()}
        evidence['interface_differences'] = [*evidence.get('interface_differences', []),
                                             naming.table(self.layout, naming_extra)]
        if self.qualification:
            from qualification import validate
            manifest = Path(self.qualification)
            qualified = validate(manifest, BINARY)
            evidence['qualification_manifest'] = str(manifest.relative_to(ROOT))
            evidence['qualification_manifest_sha256'] = sha(manifest)
            evidence['source_set_sha256'] = qualified['source_set_sha256']
        adaptations_path = ROOT / 'migration/upstream-review/adaptations.json'
        if adaptations_path.exists():
            fixes = json.loads(adaptations_path.read_text()).get(source, [])
            if fixes: evidence['adopted_fixes'] = fixes
        receipt = {
            "source": source,
            "source_sha256": sha(SOURCE / source),
            "upstream_commit": LEDGER["upstream_commit"],
            "passed": True,
            "binary_sha256": self.binary_sha256,
            "binary_path": str(BINARY),
            "rust_files": {name: sha(ROOT / name) for name in rust_files},
            "reference_python": platform.python_version(),
            "native_calls": self.calls,
            "seconds": time.monotonic() - self.started,
            **evidence,
        }
        (ROOT / f"migration/receipts/{self.name}.json").write_text(
            json.dumps(receipt, ensure_ascii=True, indent=2) + "\n"
        )
        print(json.dumps(receipt))


def upstream_tests(*paths, check=None):
    """Run unchanged upstream test files; callers substitute only ported APIs.

    A check with an upstream_naming.py table runs a temporary copy of the
    pinned tests with its recorded name translations applied.
    """
    import pytest
    import upstream_naming
    if upstream_naming.table(check):
        with upstream_naming.isolated(paths, check) as (_, copies):
            return _upstream_tests(pytest, paths, copies)
    return _upstream_tests(pytest, paths, [str(SOURCE / path) for path in paths])


def _upstream_tests(pytest, paths, locations):
    class Reports:
        passed = 0
        skipped = 0

        def pytest_runtest_logreport(self, report):
            if report.when == "call" and report.passed:
                self.passed += 1
            if report.skipped:
                self.skipped += 1

    reports = Reports()
    result = pytest.main(["-q", "-p", "no:cacheprovider", *locations], plugins=[reports])
    assert result == 0, f"Original upstream tests failed: {result}"
    return {"paths": paths, "passed": reports.passed, "skipped": reports.skipped}
