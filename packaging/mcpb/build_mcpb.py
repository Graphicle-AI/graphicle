#!/usr/bin/env python3
"""Build the MCP bundle (.mcpb) of one Graphicle release archive.

    python3 packaging/mcpb/build_mcpb.py dist/graphicle-0.1.2-aarch64-apple-darwin.tar.gz \\
        --version 0.1.2 --out dist/mcpb

An MCP bundle (https://github.com/modelcontextprotocol/mcpb) is a zip file
with a manifest.json at its root, in the MCP bundle format, which tells an
MCP client how to run the server the bundle holds. The bundle of the archive
graphicle-<version>-<target>.tar.gz (a .zip for Windows) is
OUT/graphicle-<version>-<target>.mcpb, and it holds

    manifest.json   the manifest (MCPB manifest version 0.3): a binary server
                    for this target's operating system that runs
                    `graphicle mcp --project <folder> --allow-learning`, as
                    `graphicle setup` connects a desktop client, on the folder
                    the user picks (the directory option project_folder)
    server/         the archive's program, graphicle (graphicle.exe on
                    Windows), executable. graphicle-graph stays out:
                    `graphicle mcp` links the graph engine and never runs it
    LICENSE, NOTICE, NOTICE.md and licenses/
                    the archive's licence and notices, which go wherever the
                    program goes

The server serves a project that `graphicle sync` has set up; it does not
index a folder by itself, and the manifest's long description says what to
run first. The entries are written in a fixed order, all with the archive's
own file time, the program with mode 0755 and every other file with 0644, so
the same archive gives the same bundle with the same Python and zlib. The
bundle's SHA-256 is printed as sha256sum prints it; render_server_json.py
records it in server.json.
"""
import argparse
import calendar
import hashlib
import json
import os
import re
import stat
import sys
import tarfile
import time
import zipfile
from pathlib import Path

# The public repository, which the manifest links to.
REPOSITORY = 'Graphicle-AI/graphicle'
# The release targets, in the order server.json lists them (the most common
# machine of each operating system first): the operating system as the
# manifest's compatibility.platforms names it, and what the programs run on
# (packaging/README.md).
TARGETS = {
    'aarch64-apple-darwin': ('darwin', 'macOS 11 or later on Apple silicon'),
    'x86_64-apple-darwin': ('darwin', 'macOS 11 or later on Intel'),
    'x86_64-pc-windows-msvc': ('win32', 'Windows on x86-64 (preview)'),
    'x86_64-unknown-linux-gnu': ('linux', 'Linux on x86-64 with glibc 2.35 or later'),
    'aarch64-unknown-linux-gnu': ('linux', 'Linux on Arm64 with glibc 2.35 or later'),
}
# The manifest's description, which server.json repeats: the registry allows
# 100 characters.
DESCRIPTION = 'Code graph and project memory for coding agents, cited to exact files and lines, on your machine.'
VERSION = re.compile(r'\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?')
# The archive's licence and notices that go beside the program: the first two
# must be there, NOTICE.md and licenses/ are taken when the archive has them.
LEGAL = ['LICENSE', 'NOTICE']
LEGAL_EXTRA = ['NOTICE.md']
LICENCES = 'licenses/'
# The files a bundle takes from an archive stay below this size together.
MAX_TOTAL = 1 << 30


def fail(message):
    if os.environ.get('GITHUB_ACTIONS') == 'true':
        print(f'::error::{message}', flush=True)
    sys.exit(message)


def bundle_name(version, target):
    return f'graphicle-{version}-{target}.mcpb'


def program_name(target):
    return 'graphicle.exe' if TARGETS[target][0] == 'win32' else 'graphicle'


def archive_target(name, version):
    """The target of graphicle-<version>-<target>.tar.gz (.zip for Windows)."""
    prefix = f'graphicle-{version}-'
    for suffix in ('.tar.gz', '.zip'):
        if name.startswith(prefix) and name.endswith(suffix):
            target = name[len(prefix):-len(suffix)]
            if target not in TARGETS:
                fail(f'{name}: {target!r} is not a release target ({", ".join(TARGETS)})')
            expected = '.zip' if TARGETS[target][0] == 'win32' else '.tar.gz'
            if suffix != expected:
                fail(f'{name}: the {target} archive is a {expected}')
            return target, name[:-len(suffix)]
    fail(f'{name} is not graphicle-{version}-<target>.tar.gz or .zip')


def inside(name, root):
    """A member's path inside the archive's one folder, root ('' for the
    folder itself). Every member must be in it, under a plain relative name."""
    path = name[2:] if name.startswith('./') else name
    parts = path.rstrip('/').split('/')
    if '\\' in path or path.startswith('/') or any(part in ('', '.', '..') for part in parts):
        fail(f'archive member {name!r} has an unsafe name')
    if parts[0] != root:
        fail(f'archive member {name!r} is outside the folder {root}/')
    return '/'.join(parts[1:])


def wanted(relative, program):
    return relative in (program, *LEGAL, *LEGAL_EXTRA) or relative.startswith(LICENCES)


def keep(files, name, relative, mtime, size, read):
    if relative in files:
        fail(f'the archive holds {relative} twice')
    if size + sum(len(data) for _, data in files.values()) > MAX_TOTAL:
        fail(f'the files to bundle pass {MAX_TOTAL} bytes at archive member {name}')
    files[relative] = (mtime, read())


def read_tar(archive, root, program):
    files = {}
    with tarfile.open(archive, 'r:gz') as tar:
        for member in tar:
            relative = inside(member.name, root)
            if not relative or member.isdir() or not wanted(relative, program):
                continue
            if not member.isfile():
                fail(f'archive member {member.name} is not a regular file')
            keep(files, member.name, relative, member.mtime, member.size, lambda: tar.extractfile(member).read())
    return files


def read_zip(archive, root, program):
    files = {}
    with zipfile.ZipFile(archive) as bundle:
        for info in bundle.infolist():
            relative = inside(info.filename, root)
            if not relative or info.is_dir() or not wanted(relative, program):
                continue
            mode = info.external_attr >> 16
            if info.create_system == 3 and stat.S_IFMT(mode) not in (0, stat.S_IFREG):
                fail(f'archive member {info.filename} is not a regular file')
            keep(files, info.filename, relative, calendar.timegm(info.date_time + (0, 0, 0)), info.file_size,
                 lambda: bundle.read(info))
    return files


def install_hint(version, platform):
    """How to install the graphicle command: this version's installer, or
    Homebrew's current formula on macOS."""
    base = f'https://github.com/{REPOSITORY}/releases/download/v{version}'
    if platform == 'darwin':
        return '`brew install graphicle-ai/tap/graphicle`'
    if platform == 'win32':
        return f"`$env:GRAPHICLE_VERSION = '{version}'; irm {base}/install.ps1 | iex` in PowerShell"
    return f'`curl -fsSL {base}/install.sh | sh -s -- --version {version}`'


def long_description(version, target):
    platform, runs_on = TARGETS[target]
    docs = f'https://github.com/{REPOSITORY}/blob/v{version}/docs'
    return '\n\n'.join([
        'Graphicle maps your repository into a code graph, keeps its documents, decisions and agent sessions '
        'as project memory, and gives your agent cited excerpts from both: the exact files and lines. It runs '
        'on your machine, with no account, no API key and no telemetry.',
        '**Set up the project first.** This bundle runs the Graphicle MCP server; it does not index a folder '
        f'by itself. Install the `graphicle` command ({install_hint(version, platform)}; other ways: '
        f'{docs}/install.md), run `graphicle sync` once in your project\'s folder, then choose that folder as '
        'the project folder. Until the folder is synced, the server exits when it starts and its log says what '
        'to run. Before each tool call it syncs a few changed files; after larger changes, run `graphicle sync` '
        'again.',
        'The server reads Graphicle\'s default data folder and starts as `graphicle setup` connects a desktop '
        'client: `graphicle mcp --project <folder> --allow-learning`. The agent may propose memory drafts; a '
        'person reviews each one in the app (`graphicle open`, under Memory) before recall uses it. If '
        '`graphicle setup` already connected this client, you need no bundle as well (`graphicle disconnect '
        'claude` removes Claude Desktop\'s entry). To use another data folder, connect the client with '
        '`graphicle connect` instead.',
        f'This bundle is Graphicle {version} for {runs_on}. Keep it and the `graphicle` command at the same '
        f'version. How the server works: {docs}/mcp.md',
    ])


def manifest(version, target):
    """The bundle's manifest.json (MCPB manifest version 0.3)."""
    program = program_name(target)
    return {
        'manifest_version': '0.3',
        'name': 'graphicle',
        'display_name': 'Graphicle',
        'version': version,
        'description': DESCRIPTION,
        'long_description': long_description(version, target),
        'author': {'name': 'KarLex AI, Inc.', 'url': 'https://www.karlexai.com'},
        'repository': {'type': 'git', 'url': f'https://github.com/{REPOSITORY}'},
        'homepage': f'https://github.com/{REPOSITORY}',
        'documentation': f'https://github.com/{REPOSITORY}/blob/v{version}/docs/mcp.md',
        'support': f'https://github.com/{REPOSITORY}/issues',
        'server': {
            'type': 'binary',
            'entry_point': f'server/{program}',
            'mcp_config': {
                'command': '${__dirname}/server/' + program,
                'args': ['mcp', '--project', '${user_config.project_folder}', '--allow-learning'],
                'env': {},
            },
        },
        # The tools depend on the server's options and are listed at run time.
        'tools_generated': True,
        'keywords': ['code graph', 'project memory', 'coding agents', 'code search', 'citations', 'local'],
        'license': 'Apache-2.0',
        'compatibility': {'platforms': [TARGETS[target][0]]},
        'user_config': {
            'project_folder': {
                'type': 'directory',
                'title': 'Project folder',
                'description': 'The folder of your project. Run `graphicle sync` in it once before you use '
                               'the server.',
                'required': True,
            },
        },
    }


def sha256(path):
    digest = hashlib.sha256()
    with open(path, 'rb') as stream:
        for block in iter(lambda: stream.read(1 << 20), b''):
            digest.update(block)
    return digest.hexdigest()


def write_bundle(path, entries, mtime):
    """Write entries (name, mode, bytes) in their order, every time at mtime
    (UTC; the format holds 1980 to 2107), files only, as `mcpb pack` does."""
    stamp = time.gmtime(min(max(mtime, 315532800), 4354819198))[:6]
    partial = path.with_name(path.name + '.part')
    with zipfile.ZipFile(partial, 'w') as bundle:
        for name, mode, data in entries:
            entry = zipfile.ZipInfo(name, date_time=stamp)
            entry.create_system = 3
            entry.external_attr = (stat.S_IFREG | mode) << 16
            bundle.writestr(entry, data, compress_type=zipfile.ZIP_DEFLATED, compresslevel=9)
    os.replace(partial, path)


def build(archive, version, out):
    target, root = archive_target(archive.name, version)
    program = program_name(target)
    files = (read_zip if archive.name.endswith('.zip') else read_tar)(archive, root, program)
    missing = [name for name in (program, *LEGAL) if name not in files]
    if missing:
        fail(f'{archive.name} has no {", ".join(missing)} in {root}/')
    mtime, binary = files[program]
    text = json.dumps(manifest(version, target), indent=2, ensure_ascii=False) + '\n'
    entries = [('manifest.json', 0o644, text.encode('utf-8')), (f'server/{program}', 0o755, binary)]
    # Every other file is a document, 0644 whatever the archive says.
    entries += [(name, 0o644, files[name][1]) for name in (*LEGAL, *LEGAL_EXTRA) if name in files]
    entries += [(name, 0o644, data) for name, (_, data) in sorted(files.items()) if name.startswith(LICENCES)]
    out.mkdir(parents=True, exist_ok=True)
    path = out / bundle_name(version, target)
    write_bundle(path, entries, mtime)
    print(f'{path.name}: {path.stat().st_size} bytes, {len(entries)} files; server/{program} {len(binary)} bytes, '
          f'sha256 {hashlib.sha256(binary).hexdigest()}', file=sys.stderr)
    return path


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('archive', type=Path, help='graphicle-<version>-<target>.tar.gz, or .zip for Windows')
    parser.add_argument('--version', required=True, help='the release version, without the tag\'s "v"')
    parser.add_argument('--out', type=Path, required=True, help='the folder to write the bundle to')
    args = parser.parse_args()
    if not VERSION.fullmatch(args.version):
        fail(f'--version {args.version!r} is not a version such as 0.1.2')
    if not args.archive.is_file():
        fail(f'{args.archive} is not a file')
    path = build(args.archive, args.version, args.out)
    print(f'{sha256(path)}  {path.name}')


if __name__ == '__main__':
    main()
