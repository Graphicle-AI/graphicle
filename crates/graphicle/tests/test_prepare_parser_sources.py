"""Tests of tools/prepare_parser_sources.py on synthetic bundles: which members
it keeps, the patch applier ported from tree-sitter-language-pack's build
script, the refusals and the receipt check. No network, no zstd."""
import io
import json
import sys
import tarfile
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'tools'))
import prepare_parser_sources as tool  # noqa: E402

SCANNER = ''.join(f'line {n}\n' for n in range(1, 21)).replace(
    'line 10\n', '    for (; size < BUFFER_SIZE; ++i) {\n')
PATCH = """--- a/src/scanner.c
+++ b/src/scanner.c
@@ -8,5 +8,6 @@
 line 8
 line 9
-    for (; size < BUFFER_SIZE; ++i) {
+    // Two bytes per iteration. ~keep
+    for (; size + 2 <= BUFFER_SIZE; ++i) {
 line 11
 line 12
"""


def bundle(entries):
    """An uncompressed tar stream: entries maps names to text, None for a folder."""
    data = io.BytesIO()
    with tarfile.open(fileobj=data, mode='w') as tar:
        for name, text in entries.items():
            info = tarfile.TarInfo(name)
            info.mtime = 1_700_000_000
            if text is None:
                info.type = tarfile.DIRTYPE
                tar.addfile(info)
            else:
                body = text.encode()
                info.size = len(body)
                tar.addfile(info, io.BytesIO(body))
    data.seek(0)
    return data


def grammar_bundle(scanner=SCANNER):
    return bundle({
        'parsers/': None,
        'parsers/python/': None,
        'parsers/python/src/parser.c': 'int python;\n',
        'parsers/python/src/scanner.c': scanner,
        'parsers/python/queries/highlights.scm': '(identifier) @variable\n',
        'parsers/soql/src/parser.c': 'int soql;\n',
        'parsers/soql/queries/highlights.scm': '(soql) @keyword\n',
        'parsers/elm/src/parser.c': 'int elm;\n',
        'patches/README.md': 'patches\n',
        'patches/python/overflow.patch': PATCH,
        'patches/soql/overflow.patch': PATCH,
        'sources/language_definitions.json': '{}\n',
    })


def test_str_lines_follow_rust():
    assert tool.rust_lines('') == []
    assert tool.rust_lines('a\n') == ['a']
    assert tool.rust_lines('a\n\n') == ['a', '']
    assert tool.rust_lines('a\r\nb') == ['a', 'b']
    assert tool.rust_lines('a\r') == ['a\r']
    assert tool.rust_lines('\n') == ['']


def test_a_patch_applies_once_even_when_its_hunk_moved(tmp_path):
    grammar = tmp_path / 'python'
    (grammar / 'src').mkdir(parents=True)
    scanner = grammar / 'src/scanner.c'
    moved = 'extra 1\nextra 2\n' + SCANNER
    scanner.write_text(moved)
    patch = tmp_path / 'overflow.patch'
    patch.write_text(PATCH)
    assert tool.apply_patch(grammar, patch) is True
    patched = scanner.read_text()
    assert 'size + 2 <= BUFFER_SIZE' in patched and 'size < BUFFER_SIZE' not in patched
    assert patched.startswith('extra 1\nextra 2\nline 1\n') and patched.endswith('line 20\n')
    assert len(patched.splitlines()) == len(moved.splitlines()) + 1
    # Already applied: reverse-applies, so it is left unchanged.
    assert tool.apply_patch(grammar, patch) is False
    assert scanner.read_text() == patched


def test_crlf_and_a_missing_final_newline_are_kept(tmp_path):
    grammar = tmp_path / 'python'
    (grammar / 'src').mkdir(parents=True)
    scanner = grammar / 'src/scanner.c'
    scanner.write_bytes(SCANNER.rstrip('\n').replace('\n', '\r\n').encode())
    patch = tmp_path / 'overflow.patch'
    patch.write_text(PATCH)
    assert tool.apply_patch(grammar, patch) is True
    raw = scanner.read_bytes()
    assert b'\r\n    // Two bytes per iteration. ~keep\r\n' in raw
    assert raw.endswith(b'line 20') and b'\n' not in raw.replace(b'\r\n', b'')


def test_a_patch_that_neither_applies_nor_reverse_applies_is_an_error(tmp_path):
    grammar = tmp_path / 'python'
    (grammar / 'src').mkdir(parents=True)
    (grammar / 'src/scanner.c').write_text(SCANNER.replace('line 9\n', 'line nine\n'))
    patch = tmp_path / 'overflow.patch'
    patch.write_text(PATCH)
    with pytest.raises(ValueError, match='does not reverse-apply either'):
        tool.apply_patch(grammar, patch)


def test_patch_targets_stay_inside_the_grammar(tmp_path):
    assert tool.diff_target(PATCH) == 'src/scanner.c'
    with pytest.raises(ValueError, match='outside the grammar root'):
        tool.diff_target(PATCH.replace('+++ b/src/scanner.c', '+++ b/../../etc/passwd'))
    with pytest.raises(ValueError, match='exactly one'):
        tool.diff_target(PATCH + PATCH)


def test_prepare_keeps_what_the_build_reads_and_applies_the_patches(tmp_path):
    root = tmp_path / '.tslp-root'
    receipt = tool.prepare(grammar_bundle(), root, ['python'], 'f' * 64, 'test bundle')
    files = sorted(receipt['files'])
    assert files == ['parsers/python/queries/highlights.scm', 'parsers/python/src/parser.c',
                     'parsers/python/src/scanner.c', 'parsers/soql/queries/highlights.scm',
                     'patches/README.md', 'patches/python/overflow.patch', 'patches/soql/overflow.patch',
                     'sources/language_definitions.json']
    # Unselected grammars keep only their queries; only selected ones are patched.
    assert not (root / 'parsers/soql/src').exists() and not (root / 'parsers/elm').exists()
    assert receipt['patched'] == ['parsers/python/src/scanner.c']
    assert 'size + 2 <= BUFFER_SIZE' in (root / 'parsers/python/src/scanner.c').read_text()
    assert (root / '.gitignore').read_text() == '*\n'
    assert json.loads((root / 'PREPARED.json').read_text())['languages'] == ['python']
    assert [path.name for path in tmp_path.iterdir()] == ['.tslp-root']  # no staging folder left


def test_a_prepared_folder_is_verified_and_never_changed(tmp_path, monkeypatch):
    root = tmp_path / '.tslp-root'
    receipt = tool.prepare(grammar_bundle(), root, ['python'], tool.SHA256, 'test bundle')
    assert tool.verify(root, ['python'])['state'] == 'verified'
    # A language added to TSLP_LANGUAGES later needs a new folder.
    with pytest.raises(SystemExit, match='now also names soql'):
        tool.verify(root, ['python', 'soql'])
    (root / 'parsers/python/src/scanner.c').write_text(SCANNER)
    with pytest.raises(SystemExit, match='parsers/python/src/scanner.c changed'):
        tool.verify(root, ['python'])
    assert (root / 'parsers/python/src/scanner.c').read_text() == SCANNER
    assert receipt['files']


def test_a_folder_prepared_another_way_is_accepted_only_when_complete(tmp_path):
    root = tmp_path / '.tslp-root'
    (root / 'parsers/python/src').mkdir(parents=True)
    (root / 'parsers/python/src/parser.c').write_text('int python;\n')
    (root / 'patches').mkdir()
    (root / 'sources').mkdir()
    with pytest.raises(SystemExit, match='lacks sources/language_definitions.json'):
        tool.verify(root, ['python'])
    (root / 'sources/language_definitions.json').write_text('{}\n')
    assert tool.verify(root, ['python'])['state'].startswith('present; not prepared by this tool')


@pytest.mark.parametrize('name, kind', [('../escape.c', tarfile.REGTYPE), ('/abs.c', tarfile.REGTYPE),
                                        ('parsers/python/src/link.c', tarfile.SYMTYPE)])
def test_unsafe_bundle_entries_are_refused(tmp_path, name, kind):
    data = io.BytesIO()
    with tarfile.open(fileobj=data, mode='w') as tar:
        info = tarfile.TarInfo(name)
        info.type = kind
        info.linkname = '/etc/passwd' if kind == tarfile.SYMTYPE else ''
        tar.addfile(info, io.BytesIO(b''))
    data.seek(0)
    with pytest.raises(SystemExit, match='Refusing bundle entry'):
        tool.prepare(data, tmp_path / '.tslp-root', ['python'], 'f' * 64, 'test bundle')
    assert list(tmp_path.iterdir()) == []


def test_a_language_the_bundle_lacks_is_an_error(tmp_path):
    with pytest.raises(SystemExit, match='parsers/cobol/src/parser.c'):
        tool.prepare(grammar_bundle(), tmp_path / '.tslp-root', ['python', 'cobol'], 'f' * 64, 'test bundle')
    assert list(tmp_path.iterdir()) == []


def test_languages_come_from_every_config_that_names_this_folder(tmp_path):
    def config(folder, root, languages):
        path = tmp_path / folder / '.cargo/config.toml'
        path.parent.mkdir(parents=True)
        path.write_text(f'[env]\n# comment\nPROJECT_ROOT = {{ value = "{root}", relative = true }}\n'
                        f'TSLP_LINK_MODE = "static"\nTSLP_LANGUAGES = "{languages}"\n')

    config('crates/graphicle', '../../.tslp-root', 'python,rust,go')
    config('crates/graphicle-memory', '../../.tslp-root', 'python,yaml')
    config('elsewhere', '../other-root', 'cobol')
    languages, users = tool.selected_languages(tmp_path, tmp_path / '.tslp-root')
    assert languages == ['python', 'rust', 'go', 'yaml']
    assert users == ['crates/graphicle/.cargo/config.toml', 'crates/graphicle-memory/.cargo/config.toml']


def test_the_crates_name_this_folder_and_agree_on_the_languages():
    base = ROOT.parents[1]
    languages, users = tool.selected_languages(base, base / tool.ROOT_NAME)
    assert {'crates/graphicle/.cargo/config.toml', 'crates/graphicle-memory/.cargo/config.toml'} <= set(users)
    assert len(languages) == 49 and languages[0] == 'python'
