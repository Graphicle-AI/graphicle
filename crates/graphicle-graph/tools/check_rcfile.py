#!/usr/bin/env python3
"""Compare the native `.graphiclerc` parser and language overrides with rcfile.py.

The reference keeps its overrides per thread, and the modules that read them
(detect, extract, watch) hold their own references to its functions. The
unchanged upstream tests therefore run the reference, and every rcfile call
they make on the main thread is repeated on the native side with the same
active overrides and compared: values, errors and the warning a malformed
file earns. Thread isolation is a Rust unit test (src/rcfile.rs); calls from
other threads run the reference only.
"""
import contextlib, io, os, random, sys, tempfile, threading
from pathlib import Path
from parity import Native, SOURCE, upstream_tests
import naming
from naming import to_upstream
import graphify.rcfile as reference

native = Native('rcfile')
originals = {name: getattr(reference, name) for name in (
    '_normalise_ext', 'parse_language_value', 'load_graphifyrc', 'set_language_overrides',
    'get_language_overrides', 'activate_language_overrides', 'effective_suffix', 'cache_salt')}
counts = {}
lock = threading.RLock()
# rcfile functions call one another (activation sets and reads the overrides):
# only the outermost call of a thread is compared.
active = threading.local()


def native_call(function, **kwargs):
    native.use_working_directory(os.getcwd())
    reply = native.call('rcfile', function=naming.NATIVE_FUNCTIONS.get(function, function), **kwargs)
    if not reply['ok']:
        return None, to_upstream(reply['error']), []
    value = reply['result']
    return to_upstream(value['value']), None, [to_upstream(w) for w in value['warnings']]


def sync():
    """Give the native side the overrides the reference has active now."""
    native_call('set_language_overrides', mapping=originals['get_language_overrides']())


def compared(name, plain=lambda v: v):
    original = originals[name]

    def invoke(*args, **kwargs):
        if threading.current_thread() is not threading.main_thread() or getattr(active, 'inside', False):
            return original(*args, **kwargs)
        with lock:
            stderr = io.StringIO()
            active.inside = True
            try:
                with contextlib.redirect_stderr(stderr):
                    expected, error = original(*args, **kwargs), None
            except ValueError as exc:
                expected, error = None, str(exc)
            finally:
                active.inside = False
            printed = stderr.getvalue()
            if printed:
                print(printed, end='', file=sys.stderr)
            arguments = dict(zip(original.__code__.co_varnames, args), **kwargs)
            request = {key: str(value) if isinstance(value, Path) else value
                       for key, value in arguments.items()}
            if name in ('effective_suffix', 'cache_salt', 'get_language_overrides'):
                sync()
            if name == 'set_language_overrides':
                request = {'mapping': arguments.get('mapping')}
            roots = [Path(request['root'])] if 'root' in request else []
            with naming.native_inputs(*roots):
                actual, actual_error, warnings = native_call(name, **request)
            counts[name] = counts.get(name, 0) + 1
            if name == 'set_language_overrides':
                expected, actual = originals['get_language_overrides'](), native_call('get_language_overrides')[0]
            if (error is None) != (actual_error is None) or (error is not None and actual_error != error):
                native.fail(function=name, arguments=request, expected_error=error, actual_error=actual_error)
            if error is None and plain(expected) != actual:
                native.fail(function=name, arguments=request, expected=plain(expected), actual=actual)
            if [line for line in printed.splitlines() if line] != warnings:
                native.fail(function=name, arguments=request, expected_warnings=printed, actual_warnings=warnings)
            if error is not None:
                raise ValueError(error)
            return expected
    return invoke


class WarnedRoots(set):
    """The reference's warning dedup set; clearing it clears the native one."""
    def clear(self):
        super().clear()
        with lock:
            native_call('_warned_roots.clear')


try:
    reference._warned_roots = WarnedRoots(reference._warned_roots)
    for name in originals:
        setattr(reference, name, compared(name))
    tests = upstream_tests('tests/test_language_overrides.py', 'tests/test_language_override_lifecycle.py',
                           'tests/test_pr10_watch_overrides.py')
    for name, fn in originals.items():
        setattr(reference, name, compared(name))
    rng = random.Random(2961)
    values = ['', ' ', '.', '..', '.php', '.PHP', ' .md ', '. php', '.phhp', 'php', 'PHP', 'php script',
              'c++', 'C#', 'objective-c', 'klingon', 'ts', '.inc', 'inc', ' php', 'ﬁle', '.İ']
    names = [n for n, _ in sorted(reference.LANGUAGE_ALIASES.items())]
    for _ in range(300):
        value = rng.choice(values + names) + rng.choice(['', '', ' ', '\t'])
        with contextlib.suppress(ValueError):
            reference.parse_language_value(value)
        reference._normalise_ext(rng.choice(values))
    with tempfile.TemporaryDirectory(prefix='graphify-rcfile-parity-') as directory:
        root = Path(directory)
        lines = ['# comment', '', 'viz_node_limit=5', 'viz_node_limit=-1', 'viz_node_limit=1_000',
                 'viz_node_limit=x', 'language.inc=php', 'language..INC=.php', 'language.=php',
                 'language. tpl =typescript', 'language.inc=klingon', 'language.m=.mm', 'no equals',
                 'future_option=1', 'language.h=c++', 'language.inc=pascal', '  language.tpl = .ts  ']
        for _ in range(200):
            text = '\n'.join(rng.choices(lines, k=rng.randrange(0, 6))) + rng.choice(['', '\n', '\r\n'])
            (root / '.graphifyrc').write_text(text, encoding='utf-8')
            with contextlib.suppress(ValueError):
                reference.load_graphifyrc(root)
            reference._warned_roots.clear()
            reference.activate_language_overrides(root)
            for path in ['a.inc', 'x/a.INC', 'b.tpl', 'c.h', 'd.m', 'e', '.inc', 'f.', 'g.php']:
                reference.effective_suffix(path)
                reference.cache_salt(path)
        (root / '.graphifyrc').write_bytes(b'language.inc=php\n\xff\n')
        reference._warned_roots.clear()
        reference.activate_language_overrides(root)
    reference.set_language_overrides(None)
finally:
    for name, fn in originals.items():
        setattr(reference, name, fn)
    native.close()
native.receipt('graphify/rcfile.py', ['src/rcfile.rs', 'src/par.rs'], checked=counts, upstream_tests=tests,
    adaptations=['The engine reads `.graphiclerc` (reference `.graphifyrc`); fixtures are mirrored under that name for each native call (tools/naming.py INPUT_FILES).',
                 'Overrides are per thread in both. The reference hands them to pool workers through an initializer; the native `par` helper installs the caller\'s snapshot in each worker, and a thread that never set its own reads the overrides most recently set in the process. Thread isolation is checked by Rust unit tests; reference calls made off the main thread are not repeated natively.',
                 'The malformed-file warning is returned to JSON callers and printed by the CLI, once per root as upstream.'])
