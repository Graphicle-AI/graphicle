#!/usr/bin/env python3
"""Prepare the grammar pack's parser sources once, checksum-verified.

The memory library links tree-sitter-language-pack 1.20.0, which compiles the
grammars that TSLP_LANGUAGES names (in the crates' .cargo/config.toml) from C
sources under PROJECT_ROOT: `.tslp-root`, at the repository root. This
tool fills that folder from the pack's release bundle, parser-sources-1.20.0
.tar.zst (36.6 MB, pinned below by SHA-256): the selected grammars' sources,
every grammar's queries (the pack embeds all of them), the pack's
memory-safety patches, applied the way its build script applies them, and its
language definitions, about 540 MB in all. Builds then need no network, and a
build with nothing changed rebuilds nothing.

Without the folder, the pack's build script downloads the bundle itself and
unpacks all of it (about 2 GB) into the build folder. Its cache check then
looks for _parsers/<language> while the bundle unpacks to
_parsers/parsers/<language>, so every Cargo command downloads, unpacks and
recompiles again, and no build works offline, even with --offline.

    python3 crates/graphicle/tools/prepare_parser_sources.py
    python3 crates/graphicle/tools/prepare_parser_sources.py --bundle parser-sources-1.20.0.tar.zst

Run it again to verify the folder against its PREPARED.json. It never changes
or removes an existing folder: after changing TSLP_LANGUAGES, delete the folder
and run it again. Decompressing needs Python 3.14 (compression.zstd), the
zstandard module or the zstd program.
"""
import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.error
import urllib.request
from pathlib import Path

VERSION = '1.20.0'
BUNDLE = f'parser-sources-{VERSION}.tar.zst'
URL = f'https://github.com/xberg-io/tree-sitter-language-pack/releases/download/v{VERSION}/{BUNDLE}'
SHA256 = '381b9ed7a781f822e43d3b3c8c5d030e3335f19f9c8eb7015b6a6f35a930ea54'
# The repository root, which holds crates/ and the parser sources.
BASE = Path(__file__).resolve().parents[3]
ROOT_NAME = '.tslp-root'
RECEIPT = 'PREPARED.json'
LANGUAGE = re.compile(r'[A-Za-z0-9_]+')


def sha256(path):
    digest = hashlib.sha256()
    with open(path, 'rb') as stream:
        for block in iter(lambda: stream.read(1 << 20), b''):
            digest.update(block)
    return digest.hexdigest()


def env_table(text):
    """PROJECT_ROOT and TSLP_LANGUAGES of a Cargo config's [env] table."""
    try:
        import tomllib
    except ImportError:  # Python before 3.11: the two simple forms the crates use
        root = re.search(r'^PROJECT_ROOT\s*=\s*\{\s*value\s*=\s*"([^"]*)"\s*,\s*relative\s*=\s*true\s*\}', text, re.M)
        languages = re.search(r'^TSLP_LANGUAGES\s*=\s*"([^"]*)"', text, re.M)
        return (root.group(1) if root else None), (languages.group(1) if languages else None)
    env = tomllib.loads(text).get('env', {})
    root, languages = env.get('PROJECT_ROOT'), env.get('TSLP_LANGUAGES')
    if isinstance(languages, dict):
        languages = languages.get('value')
    if isinstance(root, dict) and root.get('relative'):
        return root.get('value'), languages
    return None, languages


def selected_languages(base, root):
    """The union of TSLP_LANGUAGES over the Cargo configs (one or two folders
    below base) whose PROJECT_ROOT names root, in first-seen order."""
    configs = sorted(set(base.glob('*/.cargo/config.toml')) | set(base.glob('*/*/.cargo/config.toml')))
    languages, users = [], []
    for config in configs:
        value, names = env_table(config.read_text(encoding='utf-8'))
        if value is None or names is None:
            continue
        if os.path.normpath(str(config.parent.parent / value)) != os.path.normpath(str(root)):
            continue
        users.append(config.relative_to(base).as_posix())
        for name in names.split(','):
            name = name.strip()
            if not LANGUAGE.fullmatch(name):
                sys.exit(f'{config}: TSLP_LANGUAGES has an invalid name {name!r}')
            if name not in languages:
                languages.append(name)
    if not languages:
        sys.exit(f'No .cargo/config.toml under {base} sets PROJECT_ROOT to {root} with TSLP_LANGUAGES')
    return languages, users


def wanted(parts, languages):
    """Bundle members the build reads: the selected grammars, every grammar's
    queries (embedded for all languages), the patches and the definitions."""
    if parts[0] in ('patches', 'sources'):
        return True
    return parts[0] == 'parsers' and len(parts) > 1 and (
        parts[1] in languages or (len(parts) > 2 and parts[2] == 'queries'))


def unpack(stream, staging, languages):
    """Write the wanted members of a tar stream under staging; refuse links,
    devices and paths that leave the folder."""
    with tarfile.open(fileobj=stream, mode='r|') as tar:
        for member in tar:
            name = member.name[2:] if member.name.startswith('./') else member.name
            parts = [part for part in name.split('/') if part]
            if not parts or name.startswith('/') or '..' in parts or '\\' in name:
                sys.exit(f'Refusing bundle entry {member.name!r}: outside the parser folder')
            if not (member.isfile() or member.isdir()):
                sys.exit(f'Refusing bundle entry {member.name!r}: not a regular file or folder')
            if not wanted(parts, languages):
                continue
            target = staging.joinpath(*parts)
            if member.isdir():
                target.mkdir(parents=True, exist_ok=True)
                continue
            target.parent.mkdir(parents=True, exist_ok=True)
            with tar.extractfile(member) as source, open(target, 'wb') as out:
                shutil.copyfileobj(source, out, 1 << 20)
            os.utime(target, (member.mtime, member.mtime))


# The patch applier of tree-sitter-language-pack 1.20.0's build.rs
# (patch_grammar_sources and the functions it calls), ported line for line so
# the prepared files are exactly what its build would write: exact context,
# zero fuzz, only the hunk's position may move; a patch that reverse-applies
# is already applied.

def rust_lines(text):
    """Rust's str::lines(): split at \\n or \\r\\n, no final empty line."""
    lines = text.split('\n')
    last = lines.pop()
    lines = [line[:-1] if line.endswith('\r') else line for line in lines]
    if last:
        lines.append(last)
    return lines


def diff_target(patch_text):
    targets = [line[4:] for line in rust_lines(patch_text) if line.startswith('+++ ')]
    if len(targets) != 1:
        raise ValueError(f"expected exactly one '+++' target header, found {len(targets)}")
    path = targets[0].split('\t')[0].strip()
    stripped = path.split('/', 1)[1] if '/' in path else path
    if not stripped or stripped.startswith('/') or '..' in stripped.split('/'):
        raise ValueError(f'refusing patch target outside the grammar root: {path!r}')
    return stripped


def hunk_header(rest):
    parts = rest.split(' ')
    old = parts[0] if parts else ''
    new = parts[1][1:] if len(parts) > 1 and parts[1].startswith('+') else ''

    def span(spec):
        start, _, count = spec.partition(',')
        return int(start), (int(count) if count else 1)

    try:
        old_start, old_count = span(old)
        _, new_count = span(new)
    except ValueError:
        raise ValueError(f'malformed hunk header {rest!r}') from None
    return old_start, old_count, new_count


def parse_diff(patch_text):
    hunks = []
    lines = iter(rust_lines(patch_text))
    for line in lines:
        if not line.startswith('@@ -'):
            continue
        old_start, old_count, new_count = hunk_header(line[4:])
        body, seen_old, seen_new = [], 0, 0
        while seen_old < old_count or seen_new < new_count:
            body_line = next(lines, None)
            if body_line is None:
                raise ValueError(f'hunk at line {old_start} is truncated')
            if body_line.startswith('\\'):
                continue
            marker, content = (body_line[0], body_line[1:]) if body_line else (' ', '')
            if marker == ' ':
                seen_old, seen_new = seen_old + 1, seen_new + 1
            elif marker == '-':
                seen_old += 1
            elif marker == '+':
                seen_new += 1
            else:
                raise ValueError(f'unexpected line in hunk body: {body_line!r}')
            body.append((marker, content))
        hunks.append((old_start, body))
    if not hunks:
        raise ValueError("patch contains no '@@' hunk headers")
    return hunks


def hunk_position(lines, old, guess):
    def matches(start):
        return start + len(old) <= len(lines) and lines[start:start + len(old)] == old

    clamped = max(guess, 0)
    if matches(clamped):
        return clamped
    for distance in range(1, len(lines) + 1):
        if clamped >= distance and matches(clamped - distance):
            return clamped - distance
        if matches(clamped + distance):
            return clamped + distance
    return None


def apply_hunks(lines, hunks, reverse):
    removed, added = ('+', '-') if reverse else ('-', '+')
    result, offset = list(lines), 0
    for old_start, body in hunks:
        old = [content for marker, content in body if marker in (' ', removed)]
        new = [content for marker, content in body if marker in (' ', added)]
        guess = max(old_start - 1, 0) + offset
        position = hunk_position(result, old, guess)
        if position is None:
            raise ValueError(f'hunk at line {old_start} does not match')
        result = result[:position] + new + result[position + len(old):]
        offset += (position - guess) + (len(new) - len(old))
    return result


def apply_patch(grammar_root, patch_path):
    """True if the patch was applied, False if it was already applied."""
    patch_text = patch_path.read_bytes().decode('utf-8')
    target = grammar_root / diff_target(patch_text)
    hunks = parse_diff(patch_text)
    raw = target.read_bytes().decode('utf-8')
    crlf = '\r\n' in raw
    normalized = raw.replace('\r\n', '\n') if crlf else raw
    lines = rust_lines(normalized)
    try:
        patched = apply_hunks(lines, hunks, False)
    except ValueError as forward:
        try:
            apply_hunks(lines, hunks, True)
        except ValueError:
            raise ValueError(f'{forward} in {target}; it does not reverse-apply either') from None
        return False
    output = '\n'.join(patched) + ('\n' if normalized.endswith('\n') else '')
    if crlf:
        output = output.replace('\n', '\r\n')
    target.write_bytes(output.encode('utf-8'))
    return True


def apply_patches(root, languages):
    """Apply patches/<language>/*.patch (sorted) to each selected grammar, as
    the pack's build does before compiling it."""
    patched = []
    for language in sorted(languages):
        folder = root / 'patches' / language
        if not folder.is_dir():
            continue
        for patch in sorted(folder.glob('*.patch')):
            try:
                if apply_patch(root / 'parsers' / language, patch):
                    patched.append(f'parsers/{language}/{diff_target(patch.read_text(encoding="utf-8"))}')
            except (OSError, ValueError) as error:
                sys.exit(f'{patch.relative_to(root)} does not apply: {error}')
    return patched


def complete(root, languages):
    """What the build needs that the folder lacks."""
    lacking = [f'parsers/{language}/src/parser.c' for language in languages
               if not (root / 'parsers' / language / 'src' / 'parser.c').is_file()]
    if not (root / 'patches').is_dir():
        lacking.append('patches/')
    if not (root / 'sources' / 'language_definitions.json').is_file():
        lacking.append('sources/language_definitions.json')
    return lacking


def inventory(root):
    return {path.relative_to(root).as_posix(): sha256(path) for path in sorted(root.rglob('*'))
            if path.is_file() and path.relative_to(root).as_posix() not in (RECEIPT, '.gitignore')}


def prepare(stream, root, languages, bundle_sha256, source):
    """Unpack a verified bundle's tar stream into a new folder at root."""
    staging = Path(tempfile.mkdtemp(prefix=ROOT_NAME + '.', dir=root.parent))
    try:
        unpack(stream, staging, languages)
        lacking = complete(staging, languages)
        if lacking:
            sys.exit('The bundle lacks ' + ', '.join(lacking[:10]) + ' (a TSLP_LANGUAGES name the pack does not know?)')
        patched = apply_patches(staging, languages)
        files = inventory(staging)
        receipt = {'bundle': BUNDLE, 'source': source, 'sha256': bundle_sha256, 'languages': languages,
                   'patched': patched, 'files': files}
        (staging / RECEIPT).write_text(json.dumps(receipt, indent=1) + '\n', encoding='utf-8')
        # Ignored by Git wherever it sits, whatever the repository's own rules.
        (staging / '.gitignore').write_text('*\n', encoding='utf-8')
        os.chmod(staging, 0o755)
        if os.path.lexists(root):
            sys.exit(f'{root} appeared while preparing; leaving it unchanged')
        os.rename(staging, root)
    finally:
        if staging.exists():
            shutil.rmtree(staging)
    return receipt


def verify(root, languages):
    """Check an existing folder; never change it."""
    receipt_path = root / RECEIPT
    if not receipt_path.is_file():
        lacking = complete(root, languages)
        if lacking:
            sys.exit(f'{root} exists but lacks {", ".join(lacking[:10])}; move it aside and run this tool again')
        return {'root': str(root), 'state': 'present; not prepared by this tool, so not verified',
                'languages': len(languages)}
    receipt = json.loads(receipt_path.read_text(encoding='utf-8'))
    problems = []
    if receipt.get('sha256') != SHA256:
        problems.append(f'prepared from another bundle ({receipt.get("sha256")})')
    absent = [language for language in languages if language not in receipt.get('languages', [])]
    if absent:
        problems.append('TSLP_LANGUAGES now also names ' + ', '.join(absent))
    for name, digest in receipt.get('files', {}).items():
        path = root / name
        if not path.is_file():
            problems.append(f'{name} is missing')
        elif sha256(path) != digest:
            problems.append(f'{name} changed')
    if problems:
        sys.exit(f'{root} does not match its {RECEIPT}: ' + '; '.join(problems[:10]) +
                 f'. Delete {root} and run this tool again.')
    return {'root': str(root), 'state': 'verified', 'languages': len(languages), 'files': len(receipt['files']),
            'sha256': receipt['sha256']}


def fetch(url, destination):
    request = urllib.request.Request(url, headers={'User-Agent': 'graphicle-prepare-parser-sources'})
    try:
        with urllib.request.urlopen(request, timeout=120) as response, open(destination, 'wb') as out:
            shutil.copyfileobj(response, out, 1 << 20)
    except (urllib.error.URLError, OSError) as error:
        sys.exit(f'Could not download {url}: {error}. Download it (and {BUNDLE}.sha256) another way and pass '
                 f'--bundle {BUNDLE}.')


def decompressed(bundle):
    """A readable tar stream of the .tar.zst bundle, and the helper process if one is used."""
    try:
        from compression import zstd  # Python 3.14+
        return zstd.open(bundle, 'rb'), None
    except ImportError:
        pass
    try:
        import zstandard
        return zstandard.ZstdDecompressor().stream_reader(open(bundle, 'rb')), None
    except ImportError:
        pass
    program = shutil.which('zstd')
    if program is None:
        sys.exit('Decompressing the bundle needs Python 3.14 or later, the zstandard module or the zstd program '
                 '(for example `brew install zstd` or `apt install zstd`).')
    process = subprocess.Popen([program, '-d', '-c', '-q', '--', str(bundle)], stdout=subprocess.PIPE)
    return process.stdout, process


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--bundle', type=Path, help=f'a downloaded {BUNDLE} to use instead of fetching {URL}')
    args = parser.parse_args()
    root = BASE / ROOT_NAME
    languages, users = selected_languages(BASE, root)
    if os.path.lexists(root):
        print(json.dumps(verify(root, languages), indent=1))
        return
    with tempfile.TemporaryDirectory(prefix='parser-sources-') as scratch:
        if args.bundle:
            bundle, source = args.bundle.resolve(), str(args.bundle)
            sidecar = Path(str(bundle) + '.sha256')
            listed = sidecar.read_text().split()[:1] if sidecar.is_file() else []
        else:
            bundle, source = Path(scratch) / BUNDLE, URL
            fetch(URL + '.sha256', Path(scratch) / 'sha256')
            listed = (Path(scratch) / 'sha256').read_text().split()[:1]
            fetch(URL, bundle)
        if listed and listed[0].lower() != SHA256:
            sys.exit(f'The checksum file beside the bundle names {listed[0]}; this tool pins {SHA256}')
        actual = sha256(bundle)
        if actual != SHA256:
            sys.exit(f'{bundle} has SHA-256 {actual}; this tool pins {SHA256} (tree-sitter-language-pack {VERSION})')
        stream, process = decompressed(bundle)
        try:
            receipt = prepare(stream, root, languages, actual, source)
        finally:
            stream.close()
            if process is not None and process.wait() != 0:
                sys.exit(f'zstd failed with exit code {process.returncode}')
    print(json.dumps({'root': str(root), 'state': 'created', 'for': users, 'languages': len(languages),
                      'files': len(receipt['files']), 'patched': receipt['patched'],
                      'bytes': sum((root / name).stat().st_size for name in receipt['files']),
                      'sha256': receipt['sha256']}, indent=1))


if __name__ == '__main__':
    main()
