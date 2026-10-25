"""The single name translation table between the pinned reference and the engine.

Development-only. The comparison suite runs Python Graphify 0.9.95 unchanged
under its own names (`import graphify`, `python -m graphify`, the output folder
`graphify-out`, `GRAPHIFY_*` settings) and the unchanged upstream test files.
The engine ships Graphicle names. Checks translate names here, at the harness
boundary, and nowhere else; no comparison is loosened in any other way.

- `native_env()` builds a native process environment: every `GRAPHIFY_<X>` a
  check sets for both sides is copied to `GRAPHICLE_<X>` (fixture-only names
  excepted). Layout 'upstream', the default for JSON Lines checks, also sets
  `GRAPHICLE_OUT=graphify-out`, so native-chosen locations match the unchanged
  upstream tests. Layout 'native' leaves the engine at its true defaults.
- `native_keys()` applies the same rename to an `env` map inside a request.
- `to_upstream()` maps native names in text, paths and JSON values back to the
  reference names before an exact comparison; `to_native()` is its inverse for
  inputs the harness prepares for the native side. Under layout 'upstream' the
  output folder is not mapped back, because the native was told to use the
  reference's: only the declared messages that name the default folder
  literally (OUTPUT_FOLDER_MESSAGES) are translated, and any other '.graphicle'
  path component in a native result fails the check.
- `NATIVE_FUNCTIONS` and `NATIVE_FIELDS` map JSON Lines operation names and
  fields that mirror private upstream function names, and graph.json's
  writer stamp key (`native_fields()` renames request keys only).
- `native_copies()` / `native_inputs()` present a fixture's reference-named
  input files (`.graphify_labels.json`, `.graphifyignore`, ...) to one native
  call under the engine's names, when a check runs both implementations on
  the same fixture; `native_markers()` does the same for the cache root
  marker inside a payload.

Upstream test files that name engine artifacts are translated separately, by
recorded exact replacements (tools/upstream_naming.py).

`table()` is recorded in every receipt's interface_differences
(`parity.Native.receipt`); qualification.harness_sources() hashes this file.
"""
import contextlib
import os
import re
import shutil
import tempfile
from pathlib import Path

UPSTREAM_OUT = 'graphify-out'
NATIVE_OUT = '.graphicle'
UPSTREAM_CONFIG = '.graphify'
NATIVE_CONFIG = '.graphicle-graph'
UPSTREAM_BIN = 'graphify'
NATIVE_BIN = 'graphicle-graph'
UPSTREAM_ENV = 'GRAPHIFY_'
NATIVE_ENV = 'GRAPHICLE_'
# Portable-path marker persisted inside cache entries.
UPSTREAM_ROOT_MARKER = '$graphify-root$'
NATIVE_ROOT_MARKER = '$graphicle-root$'

# Variables that only carry the upstream prefix: read by the checks' fake
# helper executables (gws, yt-dlp, git), never by the engine or the reference.
FIXTURE_ONLY_ENV = frozenset({
    'GRAPHIFY_GWS_FIXTURE_CONTROL', 'GRAPHIFY_GWS_FIXTURE_LOG',
    'GRAPHIFY_DOWNLOAD_CONTROL', 'GRAPHIFY_DOWNLOAD_LOG',
    'GRAPHIFY_TEST_CHILD_PID',
})

# Harness variables (never read by product code). qualify_all.py keeps these
# and scrubs every other inherited GRAPHIFY_*/GRAPHICLE_* variable.
HARNESS_ENV = frozenset({
    'GRAPHICLE_ORACLE_SOURCE',           # pinned reference checkout (default: FILES.json source_root)
    'GRAPHICLE_GRAPH_BINARY',            # the native executable under test
    'GRAPHICLE_QUALIFICATION_MANIFEST',  # the source-bound build manifest
    'GRAPHICLE_GRAPH_BUILD',             # snapshot_native.py input
})

# JSON Lines operations and result fields that mirror private upstream names.
NATIVE_FUNCTIONS = {
    'resolve_graphify_paths': 'resolve_output_paths',
    '_load_graphifyignore': '_load_ignore_file',
    '_is_graphify_note': '_is_generated_note',
    'load_graphifyrc': 'load_rc_file',
}
NATIVE_FIELDS = {
    'graphify_out': 'graph_dir',
    'graphifyignore_patterns': 'ignore_patterns',
    # graph.json's writer stamp (graph metadata, 0.9.78 #4167).
    'graphify_version': 'graphicle_version',
}
NATIVE_FLAGS = {'--graphify-out': '--graph-dir'}

# Exact text tokens, reference spelling -> engine spelling. Longer tokens win
# at the same position, so every specific name is listed before the folder.
TOKENS = [
    # Output folder, configuration folder and sidecar files.
    (UPSTREAM_OUT, NATIVE_OUT),
    (UPSTREAM_CONFIG, NATIVE_CONFIG),
    ('.graphify_', '.graphicle_'),
    ('.graphifyignore', '.graphicleignore'),
    ('.graphifyrc', '.graphiclerc'),
    ('.graphify-bak', '.graphicle-bak'),
    (UPSTREAM_ROOT_MARKER, NATIVE_ROOT_MARKER),
    # Caches and logs under the user's cache folder.
    ('.cache/graphify/', '.cache/graphicle/'),
    ('.cache/graphify-rebuild.log', '.cache/graphicle/rebuild.log'),
    ('.cache/graphify-queries.log', '.cache/graphicle/queries.log'),
    # Settings and diagnostics.
    (UPSTREAM_ENV, NATIVE_ENV),
    ('[graphify', '[graphicle'),
    ('graphify: ', 'graphicle: '),
    # MCP resource URIs.
    ('graphify://', 'graphicle-graph://'),
    # Agent integration: registration markers ('# graphify' block, '## graphify'
    # sections), skill folders, rule, plugin and command files.
    ('# graphify', '# graphicle'),
    ('skills/graphify', 'skills/graphicle'),
    ('.aider/graphify', '.aider/graphicle'),
    ('rules/graphify.mdc', 'rules/graphicle.mdc'),
    ('rules/graphify.md', 'rules/graphicle.md'),
    ('steering/graphify.md', 'steering/graphicle.md'),
    ('workflows/graphify.md', 'workflows/graphicle.md'),
    ('command/graphify.md', 'command/graphicle.md'),
    ('plugins/graphify.js', 'plugins/graphicle.js'),
    # Git integration.
    ('graphify-hook-', 'graphicle-hook-'),
    ('graphify-checkout-hook-', 'graphicle-checkout-hook-'),
    ('merge.graphify.', 'merge.graphicle.'),
    ('merge=graphify', 'merge=graphicle'),
    ('graphify graph.json union merge', 'graphicle graph.json union merge'),
    # graph.html theme choice (localStorage key and the head script's id).
    ('graphify-theme', 'graphicle-theme'),
]

# User-facing text the engine rewords with its own name, engine spelling ->
# reference spelling. Used only to map native output back before an exact
# comparison; every entry is a complete renamed phrase.
PHRASES = [
    # Benchmark report (the engine labels the ratio a size ratio, not a
    # saving), tree viewer, saved query results, LLM system prompt.
    ('graphicle graph benchmark: whole corpus vs one graph answer', 'graphify token reduction benchmark'),
    ('  Size ratio:      ', '  Reduction:       '),
    ('x (a size ratio, not an agent saving)\n', 'x fewer tokens per query\n'),
    (' — Graphicle tree viewer', ' — graphify tree viewer'),
    ('contributor: "graphicle"', 'contributor: "graphify"'),
    ('You are a Graphicle semantic extraction agent.', 'You are a graphify semantic extraction agent.'),
    # Call-flow HTML export (English and Chinese).
    (' · Graphicle callflow-html</p>', ' · graphify callflow-html</p>'),
    ('Generated from Graphicle knowledge graph: ', 'Generated from graphify knowledge graph: '),
    ('由 Graphicle 知识图谱生成：', '由 graphify 知识图谱生成：'),
    ('This section comes from Graphicle community clustering.', 'This section comes from graphify community clustering.'),
    ('本节由 Graphicle 社区聚类生成。', '本节由 graphify 社区聚类生成。'),
    # Graph exports: HTML title, Obsidian tags and warnings, Cypher header.
    ('<title>Graphicle - ', '<title>graphify - '),
    ('  - graphicle/', '  - graphify/'),
    ('#graphicle/', '#graphify/'),
    (' pre-existing file(s) graphicle did not create,', ' pre-existing file(s) graphify did not create,'),
    ('// Neo4j Cypher import - generated by Graphicle', '// Neo4j Cypher import - generated by /graphify'),
    # Wiki export.
    ('*Part of the Graphicle knowledge wiki.', '*Part of the graphify knowledge wiki.'),
    ('> Auto-generated by Graphicle. Start here', '> Auto-generated by graphify. Start here'),
    ('*Generated by Graphicle*', '*Generated by [graphify](https://github.com/Graphify-Labs/graphify)*'),
]

# Words naming the program: the executable `graphicle-graph` (reference:
# `graphify`) and the agent slash command `/graphicle` (reference: `/graphify`),
# as whole words only, so paths that merely contain them are left alone.
WORDS = [
    (r'(?<![\w./-])' + re.escape(NATIVE_BIN) + r'(?![\w/-])', UPSTREAM_BIN),
    (r'(?<![\w./-])/graphicle(?![\w/-])', '/graphify'),
]

# Messages that name the engine's default output folder literally, whatever
# folder is configured, where the reference names its own literally: engine
# spelling -> reference spelling, complete phrases only. Under layout
# 'upstream' (GRAPHICLE_OUT=graphify-out) these are the only places where the
# output folder is translated; a '.graphicle' anywhere else in a native result
# means the engine fell back to its default folder, and the check fails.
OUTPUT_FOLDER_MESSAGES = [
    # reflect: the LESSONS.md banner.
    (' in .graphicle/memory/. Deterministic; no LLM.', ' in graphify-out/memory/. Deterministic; no LLM.'),
    # Obsidian export: the pre-existing-files warning.
    ('(or the default .graphicle/obsidian)', '(or the default graphify-out/obsidian)'),
    # hook-guard gemini: the BeforeTool context names the default folder.
    ('graphicle: knowledge graph at .graphicle/. For focused', 'graphify: knowledge graph at graphify-out/. For focused'),
]
# The default output folder as a path component (not .graphicle_*, -graph, ...).
_DEFAULT_OUT = re.compile(r'(?<![\w.-])' + re.escape(NATIVE_OUT) + r'(?![\w-])')
LAYOUTS = ('upstream', 'native')


def _translator(literals, words=()):
    lookup = dict(literals)
    keys = sorted(lookup, key=len, reverse=True)
    alternatives = [re.escape(key) for key in keys] + [f'(?P<w{i}>{pattern})' for i, (pattern, _) in enumerate(words)]
    pattern = re.compile('|'.join(alternatives))

    def replace(match):
        for i, (_, replacement) in enumerate(words):
            if match.group(f'w{i}') is not None:
                return replacement
        return lookup[match.group(0)]
    return lambda text: pattern.sub(replace, text)


# Layout 'native': the engine writes its own folder, translated everywhere.
_TO_UPSTREAM = _translator([(native, upstream) for upstream, native in TOKENS] + PHRASES + OUTPUT_FOLDER_MESSAGES, WORDS)
# Layout 'upstream': every token but the output folder, which only the declared
# messages translate.
_TO_UPSTREAM_CONFIGURED = _translator(
    [(native, upstream) for upstream, native in TOKENS if upstream != UPSTREAM_OUT] + PHRASES + OUTPUT_FOLDER_MESSAGES, WORDS)
_TO_NATIVE = _translator(TOKENS)
_UPSTREAM_FIELDS = {native: upstream for upstream, native in NATIVE_FIELDS.items()}


def _configured_folder(text):
    translated = _TO_UPSTREAM_CONFIGURED(text)
    if _DEFAULT_OUT.search(translated):
        raise AssertionError(
            f"native result names the default output folder {NATIVE_OUT!r} under layout 'upstream' "
            f"({NATIVE_ENV}OUT={UPSTREAM_OUT}) outside the declared OUTPUT_FOLDER_MESSAGES: {text[:2000]!r}")
    return translated


def _walk(value, text, fields):
    if isinstance(value, str):
        return text(value)
    if isinstance(value, dict):
        return {(fields.get(k) or text(k) if isinstance(k, str) else k): _walk(v, text, fields)
                for k, v in value.items()}
    if isinstance(value, list):
        return [_walk(v, text, fields) for v in value]
    if isinstance(value, tuple):
        return tuple(_walk(v, text, fields) for v in value)
    return value


def to_upstream(value, layout='upstream'):
    """Engine names -> reference names in a string or JSON value.

    Dictionary keys are translated too; renamed result fields
    (NATIVE_FIELDS) are mapped as exact keys only. With layout 'upstream'
    (the native ran with GRAPHICLE_OUT=graphify-out) the output folder is
    translated only inside OUTPUT_FOLDER_MESSAGES, and any other '.graphicle'
    path component raises AssertionError; with layout 'native' it is
    translated everywhere.
    """
    if layout not in LAYOUTS:
        raise ValueError(f'unknown layout {layout!r}')
    return _walk(value, _configured_folder if layout == 'upstream' else _TO_UPSTREAM, _UPSTREAM_FIELDS)


def to_native(value):
    """Reference names -> engine names (inputs the harness prepares)."""
    return _walk(value, _TO_NATIVE, NATIVE_FIELDS)


_MARKER_TO_NATIVE = _translator([(UPSTREAM_ROOT_MARKER, NATIVE_ROOT_MARKER)])


def native_markers(value):
    """Only the cache root marker -> the engine's, in a request payload.

    Payloads the reference relativized carry `$graphify-root$`; paths and every
    other value are left exactly as they are.
    """
    return _walk(value, _MARKER_TO_NATIVE, {})


def native_fields(value):
    """Rename request keys that mirror private upstream names (NATIVE_FIELDS).

    Only exact dictionary keys change; values (paths, labels) are untouched.
    """
    if isinstance(value, dict):
        return {NATIVE_FIELDS.get(k, k): native_fields(v) for k, v in value.items()}
    if isinstance(value, list):
        return [native_fields(v) for v in value]
    return value


@contextlib.contextmanager
def native_copies(*paths):
    """Present upstream-named input files to one native call under engine names.

    A check that runs both implementations on one fixture keeps the reference
    file (e.g. `.graphify_labels.json`); for the native call a copy under the
    engine's name (`.graphicle_labels.json`) is placed beside it when absent and
    removed afterwards, so both sides read the same content. A copy the native
    call rewrote is its own output and is kept.
    """
    created = []
    try:
        for path in map(Path, paths):
            twin = path.with_name(to_native(path.name))
            if twin != path and path.is_file() and not twin.exists():
                shutil.copy2(path, twin)
                created.append((twin, twin.read_bytes(), twin.stat().st_mtime_ns))
        yield
    finally:
        for twin, content, mtime in created:
            if twin.is_file() and twin.stat().st_mtime_ns == mtime and twin.read_bytes() == content:
                twin.unlink()


# Files a fixture may place for the code under test to read.
INPUT_FILES = ('.graphifyignore', '.graphifyrc')


def native_input_path(path):
    """A path naming a reference input file (INPUT_FILES) -> the engine's name."""
    path = Path(path)
    return path.with_name(to_native(path.name)) if path.name in INPUT_FILES else path


@contextlib.contextmanager
def native_inputs(*directories, recursive=()):
    """native_copies() for every reference-named input file in the directories.

    `directories` are searched directly (for example the ancestors of a file an
    extractor reads), `recursive` ones with their whole tree. Only folders
    below the system temporary directory are considered, so fixtures are
    mirrored and nothing else is ever written.
    """
    temp = Path(tempfile.gettempdir()).resolve()
    found = []
    def inside(folder):
        folder = Path(folder).resolve()
        return folder.is_relative_to(temp) and folder != temp
    for folder in directories:
        if inside(folder):
            found += [Path(folder) / name for name in INPUT_FILES]
    def walk(folder):
        # os.scandir, not os.walk: some upstream tests spy on os.walk.
        with os.scandir(folder) as entries:
            for entry in entries:
                if entry.is_dir(follow_symlinks=False):
                    yield from walk(entry.path)
                elif entry.name in INPUT_FILES:
                    yield Path(entry.path)
    for folder in recursive:
        if inside(folder) and Path(folder).is_dir():
            found += list(walk(folder))
    with native_copies(*found):
        yield


def native_name(name):
    """A reference GRAPHIFY_<X> setting name -> the engine's GRAPHICLE_<X>."""
    if name.startswith(UPSTREAM_ENV) and name not in FIXTURE_ONLY_ENV:
        return NATIVE_ENV + name[len(UPSTREAM_ENV):]
    return name


def native_keys(env):
    """Rename the GRAPHIFY_<X> keys of a request's `env` map."""
    if not isinstance(env, dict):
        return env
    renamed = {}
    for key, value in env.items():
        renamed[native_name(key) if isinstance(key, str) else key] = value
    return renamed


def native_env(environ, layout='upstream'):
    """The environment for a native process started by a check.

    Every GRAPHIFY_<X> is copied to GRAPHICLE_<X>; a value the check set for
    both sides wins over an inherited GRAPHICLE_<X>. With layout='upstream' the
    output folder is the reference's `graphify-out` unless GRAPHIFY_OUT is set.
    """
    if layout not in LAYOUTS:
        raise ValueError(f'unknown layout {layout!r}')
    env = dict(environ)
    for key, value in environ.items():
        if key.startswith(UPSTREAM_ENV) and key not in FIXTURE_ONLY_ENV:
            env[native_name(key)] = value
    if layout == 'upstream' and UPSTREAM_ENV + 'OUT' not in environ:
        env[NATIVE_ENV + 'OUT'] = UPSTREAM_OUT
    return env


def scrubbed(environ):
    """Drop every inherited GRAPHIFY_* and GRAPHICLE_* variable except HARNESS_ENV."""
    return {k: v for k, v in environ.items()
            if k in HARNESS_ENV or not k.startswith((UPSTREAM_ENV, NATIVE_ENV))}


# Behaviour that the name translation alone does not make identical.
DECLARED = [
    "The reference matches the literal 'graphify-out' at a few sites (detect manifest "
    "pruning, the HTML title's portable path, reflect, the hook guard's default graph, "
    "git hook defaults); there the engine matches its configured output folder or its "
    "default '.graphicle', and the HTML title's prefix match uses '.graphicle' except "
    "for the configuration folder '.graphicle-graph' (upstream's 'graphify-out' prefix "
    "never matches its '.graphify'). Under layout 'upstream' (GRAPHICLE_OUT=graphify-out) "
    "exact matches coincide; a custom output folder no longer also matches 'graphify-out'.",
    "Both upstream names ('graphify-out', '.graphify', skills/graphify, graphify.md/"
    ".mdc/.js) and engine names stay in the engine's internal skip tables.",
]


def table(layout='upstream', extra=None):
    """The translation applied by a check, for its receipt."""
    record = {
        'module': 'tools/naming.py',
        'layout': layout,
        'native_output_folder': UPSTREAM_OUT if layout == 'upstream' else NATIVE_OUT,
        'output_folder_translation': (
            f"only inside output_folder_messages; any other {NATIVE_OUT!r} path component in a "
            "native result fails the check" if layout == 'upstream' else 'everywhere'),
        'output_folder_messages': [[native, upstream] for native, upstream in OUTPUT_FOLDER_MESSAGES],
        'environment': {'copy': f'{UPSTREAM_ENV}<X> -> {NATIVE_ENV}<X>',
                        'not_copied': sorted(FIXTURE_ONLY_ENV)},
        'tokens': [[upstream, native] for upstream, native in TOKENS],
        'native_phrases': [[native, upstream] for native, upstream in PHRASES],
        'program_words': [[pattern, upstream] for pattern, upstream in WORDS],
        'functions': NATIVE_FUNCTIONS,
        'fields': NATIVE_FIELDS,
        'flags': NATIVE_FLAGS,
        'declared_differences': DECLARED,
    }
    if extra:
        record.update(extra)
    return {'name_translation': record}
