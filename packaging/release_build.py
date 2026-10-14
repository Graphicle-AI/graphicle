#!/usr/bin/env python3
"""Build and package the two Graphicle programs for one target.

Builds graphicle-graph (the graph engine, in crates/graphicle-graph/) and
graphicle (in crates/graphicle/) with the release profile and the default
features, checks that each runs and reports the expected version and the Core
edition, and that it links only operating-system libraries, then writes

    OUT/graphicle-<version>-<target>.tar.gz         a folder of the same name holding
                                                    both programs, LICENSE, NOTICE,
                                                    README.md and CHANGELOG.md, plus
                                                    NOTICE.md and licenses/ when the
                                                    root has them
    OUT/graphicle-<version>-<target>.tar.gz.sha256  "<sha256>  <archive name>"

The release workflow (.github/workflows/release.yml) runs it once per target on
a native runner. Locally:

    python3 crates/graphicle/tools/prepare_parser_sources.py   # once per checkout
    python3 packaging/release_build.py --out dist

CARGO_TARGET_DIR, when set, serves both crates; otherwise each builds in its
own target/ folder. The archive is reproducible: sorted entries, owner 0, and
file times from SOURCE_DATE_EPOCH, else from the last commit. The builds remap
the folders that Rust code would embed in the programs (the checkout, Cargo's
home, the build folder and the home folder) to fixed names (remap_flags); C
code compiled by build scripts may still name them, so publish only the
archives the release workflow builds.
"""
import argparse
import gzip
import hashlib
import json
import os
import re
import subprocess
import sys
import tarfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
# Program: (crate folder, cargo arguments that select it).
PROGRAMS = {
    'graphicle': ('crates/graphicle', ['--bin', 'graphicle']),
    'graphicle-graph': ('crates/graphicle-graph', ['-p', 'graphicle-graph']),
}
# Shipped beside the programs in every archive.
DOCS = ['LICENSE', 'NOTICE', 'README.md', 'CHANGELOG.md']
EXTRAS = ['NOTICE.md', 'licenses']
VERSION = r'\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?'
# What a release program may link besides itself: the operating system's own
# libraries. zlib (flate2's `zlib` feature) is part of every glibc
# distribution's base system.
MACOS_SYSTEM = ('/usr/lib/', '/System/Library/')
LINUX_SYSTEM = {'libc.so.6', 'libm.so.6', 'libpthread.so.0', 'libdl.so.2', 'librt.so.1', 'libutil.so.1',
                'libgcc_s.so.1', 'libz.so.1', 'ld-linux-x86-64.so.2', 'ld-linux-aarch64.so.1'}


def fail(message):
    if os.environ.get('GITHUB_ACTIONS') == 'true':
        print(f'::error::{message}', flush=True)
    sys.exit(message)


def warn(message):
    prefix = '::warning::' if os.environ.get('GITHUB_ACTIONS') == 'true' else 'warning: '
    print(prefix + message, file=sys.stderr, flush=True)


def run(command, cwd=None):
    return subprocess.run(command, cwd=cwd, capture_output=True, text=True)


def host_target():
    output = run(['rustc', '-vV']).stdout
    match = re.search(r'^host: (\S+)$', output, re.M)
    if not match:
        fail('rustc -vV printed no host line; is Rust installed?')
    return match.group(1)


def target_directory(workspace, offline):
    command = ['cargo', 'metadata', '--format-version', '1', '--no-deps', '--locked'] + (['--offline'] if offline else [])
    result = run(command, cwd=ROOT / workspace)
    if result.returncode:
        fail(f'cargo metadata failed in {workspace}/: {result.stderr.strip()[-500:]}')
    return Path(json.loads(result.stdout)['target_directory'])


def remap_flags():
    """rustc flags that replace the build machine's folders in the paths Rust
    code embeds (panic locations): the home folder, Cargo's home (the crates'
    sources), the build folder and this checkout, each as given and resolved.
    rustc applies the last prefix that matches, so the most specific comes last."""
    home = Path.home()
    folders = [(home, '/home/build'), (Path(os.environ.get('CARGO_HOME') or home / '.cargo'), '/cargo')]
    if os.environ.get('CARGO_TARGET_DIR'):
        folders.append((Path(os.environ['CARGO_TARGET_DIR']).absolute(), '/target'))
    folders.append((ROOT, '/graphicle'))
    flags = []
    for folder, name in folders:
        for form in dict.fromkeys([str(folder), str(folder.resolve())]):
            flags.append(f'--remap-path-prefix={form}={name}')
    return flags


def build(program, target, cross, offline):
    workspace, selection = PROGRAMS[program]
    command = ['cargo', 'build', '--locked', '--release', *selection]
    command += ['--target', target] if cross else []
    command += ['--offline'] if offline else []
    # CARGO_ENCODED_RUSTFLAGS (unit separators) keeps folders with spaces whole
    # and takes precedence over RUSTFLAGS, whose flags it keeps.
    env = dict(os.environ)
    flags = [flag for flag in env.pop('CARGO_ENCODED_RUSTFLAGS', '').split('\x1f') if flag]
    flags = flags or env.get('RUSTFLAGS', '').split()
    env['CARGO_ENCODED_RUSTFLAGS'] = '\x1f'.join(flags + remap_flags())
    print(f'$ (cd {workspace} && {" ".join(command)})', flush=True)
    if subprocess.run(command, cwd=ROOT / workspace, env=env).returncode:
        fail(f'building {program} failed')


def program_path(program, target, cross, offline):
    workspace, _ = PROGRAMS[program]
    folder = target_directory(workspace, offline)
    path = (folder / target / 'release' / program) if cross else (folder / 'release' / program)
    if not path.is_file():
        fail(f'{path} does not exist; build {program} first')
    return path


def reported_version(program, path):
    """The version `<program> --version` prints; graphicle must name the Core edition."""
    try:
        result = subprocess.run([str(path), '--version'], capture_output=True, text=True, timeout=120)
    except OSError as error:
        fail(f'{path} does not run on this machine ({error}); build each target on a native runner')
    text = result.stdout.strip()
    pattern = rf'{re.escape(program)} ({VERSION})' + (r' \(core\)' if program == 'graphicle' else '')
    match = re.fullmatch(pattern, text)
    if result.returncode or not match:
        fail(f'`{path.name} --version` printed {text!r} (exit {result.returncode}); expected "{pattern}"')
    return match.group(1), text


def check_linkage(path, target):
    """Fail if the program links a library outside the operating system; return
    the newest glibc symbol version it needs (Linux)."""
    if target.endswith('-apple-darwin'):
        result = run(['otool', '-L', str(path)])
        if result.returncode:
            fail(f'otool -L {path} failed: {result.stderr.strip()}')
        libraries = [line.strip().split(' (')[0] for line in result.stdout.splitlines()[1:] if line.strip()]
        foreign = [library for library in libraries if not library.startswith(MACOS_SYSTEM)]
        if foreign:
            fail(f'{path.name} links libraries outside macOS: {", ".join(foreign)}')
        return libraries, None
    if target.endswith('-linux-gnu'):
        result = run(['readelf', '--dynamic', '--version-info', '--wide', str(path)])
        if result.returncode:
            fail(f'readelf on {path} failed: {result.stderr.strip()}')
        libraries = re.findall(r'\(NEEDED\)\s+Shared library: \[([^\]]+)\]', result.stdout)
        foreign = [library for library in libraries if library not in LINUX_SYSTEM]
        if foreign:
            fail(f'{path.name} links libraries outside the base system: {", ".join(foreign)}')
        versions = re.findall(r'GLIBC_(\d+(?:\.\d+)+)', result.stdout)
        return libraries, max(versions, key=version_key) if versions else None
    fail(f'no linkage rule for target {target}')


def version_key(text):
    return tuple(int(part) for part in text.split('.'))


def accepts(release, version):
    """A program version fits a release: equal, or the release's base version
    for a prerelease (0.1.0 in a 0.1.0-rc.1 rehearsal)."""
    return version in (release, release.split('-')[0])


def source_date_epoch():
    if os.environ.get('SOURCE_DATE_EPOCH', '').isdigit():
        return int(os.environ['SOURCE_DATE_EPOCH'])
    result = run(['git', 'log', '-1', '--format=%ct'], cwd=ROOT)
    return int(result.stdout.strip()) if result.returncode == 0 and result.stdout.strip().isdigit() else 0


def root_documents(allow_missing_docs, require_licenses):
    """(path in the archive's folder, source file) of the documents each archive
    carries; checked before anything is built."""
    documents = []
    for doc in DOCS:
        if (ROOT / doc).is_file():
            documents.append((doc, ROOT / doc))
        elif allow_missing_docs:
            warn(f'the root has no {doc}; this archive is a rehearsal and must not be released')
        else:
            fail(f'the root has no {doc}, which every archive must carry')
    for extra in EXTRAS:
        source = ROOT / extra
        if source.is_file():
            documents.append((extra, source))
        elif source.is_dir():
            documents += [(path.relative_to(ROOT).as_posix(), path) for path in sorted(source.rglob('*'))
                          if path.is_file() and path.name != '.DS_Store']
    if require_licenses and not any(name.startswith('licenses/') for name, _ in documents):
        fail('the root has no licenses/ folder, so the archive would carry no third-party licence texts')
    return documents


def write_archive(archive, members, mtime):
    folders = sorted({parent.as_posix() for member, _, _ in members for parent in Path(member).parents
                      if parent != Path('.')})

    def info(name, kind, mode, size=0):
        entry = tarfile.TarInfo(name)
        entry.type, entry.mode, entry.size, entry.mtime = kind, mode, size, mtime
        entry.uid = entry.gid = 0
        entry.uname = entry.gname = ''
        return entry

    with open(archive, 'wb') as raw, \
            gzip.GzipFile(filename='', mode='wb', fileobj=raw, mtime=0, compresslevel=9) as compressed, \
            tarfile.open(fileobj=compressed, mode='w', format=tarfile.PAX_FORMAT) as tar:
        for folder in folders:
            tar.addfile(info(folder + '/', tarfile.DIRTYPE, 0o755))
        for member, source, mode in members:
            with open(source, 'rb') as stream:
                tar.addfile(info(member, tarfile.REGTYPE, mode, source.stat().st_size), stream)


def sha256(path):
    digest = hashlib.sha256()
    with open(path, 'rb') as stream:
        for block in iter(lambda: stream.read(1 << 20), b''):
            digest.update(block)
    return digest.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--target', help="Rust target triple (default: the host's)")
    parser.add_argument('--out', type=Path, default=Path('dist'), help='folder for the archive (default: dist)')
    parser.add_argument('--expect-version', help='the release version, a tag without its "v"; the programs must '
                        'report it (a prerelease such as 0.1.0-rc.1 may also report 0.1.0)')
    parser.add_argument('--build', choices=['all', 'graphicle', 'graphicle-graph', 'none'], default='all',
                        help='what to build before packaging (none: package the programs already built)')
    parser.add_argument('--no-package', action='store_true', help='only build')
    parser.add_argument('--offline', action='store_true', help='pass --offline to cargo')
    parser.add_argument('--require-licenses', action='store_true',
                        help='fail unless the root has a licenses/ folder (the release workflow sets it)')
    parser.add_argument('--allow-missing-docs', action='store_true',
                        help='package even without LICENSE, NOTICE, README.md or CHANGELOG.md (rehearsals only)')
    parser.add_argument('--max-glibc', help='Linux: fail if a program needs a newer glibc than this, such as 2.35')
    args = parser.parse_args()

    host = host_target()
    target = args.target or host
    cross = target != host
    if args.expect_version and not re.fullmatch(VERSION, args.expect_version):
        fail(f'--expect-version {args.expect_version!r} is not a version such as 0.1.0 or 0.1.0-rc.1')
    declared = re.search(r'^version\s*=\s*"([^"]+)"', (ROOT / 'crates/graphicle/Cargo.toml').read_text(), re.M)
    if args.expect_version and (not declared or not accepts(args.expect_version, declared.group(1))):
        fail(f'crates/graphicle/Cargo.toml declares version {declared and declared.group(1)}, but the release is '
             f'{args.expect_version}: set it (and Cargo.lock) before tagging')
    documents =[] if args.no_package else root_documents(args.allow_missing_docs, args.require_licenses)
    selected = {'all': list(PROGRAMS), 'graphicle': ['graphicle'], 'graphicle-graph': ['graphicle-graph'],
                'none': []}
    for program in selected[args.build]:
        build(program, target, cross, args.offline)
    if args.no_package:
        return
    programs = {program: program_path(program, target, cross, args.offline) for program in PROGRAMS}

    report = {'target': target, 'programs': {}}
    for program, path in programs.items():
        version, text = reported_version(program, path)
        libraries, glibc = check_linkage(path, target)
        if glibc and args.max_glibc and version_key(glibc) > version_key(args.max_glibc):
            fail(f'{program} needs glibc {glibc}, newer than the {args.max_glibc} this release supports')
        report['programs'][program] = {'version_output': text, 'version': version, 'bytes': path.stat().st_size,
                                       'sha256': sha256(path), 'links': libraries, 'newest_glibc': glibc}
    version = report['programs']['graphicle']['version']
    if args.expect_version:
        if not accepts(args.expect_version, version):
            fail(f'graphicle reports version {version}, but the release is {args.expect_version}: '
                 'set the version in crates/graphicle/Cargo.toml (and Cargo.lock) before tagging')
        version = args.expect_version
    engine_version = report['programs']['graphicle-graph']['version']
    if engine_version != version.split('-')[0]:
        warn(f'graphicle-graph reports version {engine_version}, the release is {version}')

    name = f'graphicle-{version}-{target}'
    members = sorted([(f'{name}/{program}', path, 0o755) for program, path in programs.items()]
                     + [(f'{name}/{relative}', source, 0o644) for relative, source in documents])
    args.out.mkdir(parents=True, exist_ok=True)
    archive = args.out / f'{name}.tar.gz'
    write_archive(archive, members, source_date_epoch())
    digest = sha256(archive)
    (args.out / f'{archive.name}.sha256').write_text(f'{digest}  {archive.name}\n')
    report.update({'version': version, 'archive': str(archive), 'sha256': digest, 'bytes': archive.stat().st_size,
                   'members': [member for member, _, _ in members]})
    print(json.dumps(report, indent=1))


if __name__ == '__main__':
    main()
