#!/usr/bin/env python3
"""Write server.json, Graphicle's entry in the official MCP Registry, for one release.

    python3 packaging/mcpb/render_server_json.py --version 0.1.2 --bundles dist/mcpb --out server.json

The entry follows the registry's schema of 2025-12-11 and lists the release's
five MCP bundles, which build_mcpb.py writes to the bundles folder, as
packages of type mcpb: each by its download address on the release (tag
v<version>), with the file's SHA-256, which MCP clients check before they
install a bundle, and stdio transport. The registry has no field for a
package's platform: the file names carry the target, each bundle's manifest
names its operating system, and the packages are listed with the most common
machine of each system first. The entry's icon is the bundles' icon.png, by its
address on raw.githubusercontent.com in the commit the GitHub Actions run
checked out, which stays the same when main moves on (outside GitHub Actions,
the address names main).

The namespace io.github.Graphicle-AI is spelled as GitHub spells the
organization: the registry grants a GitHub Actions run of this repository the
names under io.github.<owner>/ with the owner's own capitals, and compares
names case-sensitively. In a GitHub Actions run the script refuses another
repository.
"""
import argparse
import json
import os
import re
import sys
import zipfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from build_mcpb import (DESCRIPTION, ICON, ICON_SIZE, REPOSITORY, TARGETS, VERSION, bundle_name, fail,  # noqa: E402
                        icon, program_name, sha256)

SCHEMA = 'https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json'
OWNER = REPOSITORY.split('/')[0]
NAME = f'io.github.{OWNER}/graphicle'
TITLE = 'Graphicle'
# The repository's GitHub id (gh api repos/Graphicle-AI/graphicle --jq .id),
# which changes if the repository is ever deleted and created again.
REPOSITORY_ID = '1418870464'
# The registry's rules for these fields (its schema and its MCPB validator).
NAME_PATTERN = re.compile(r'[a-zA-Z0-9.-]+/[a-zA-Z0-9._-]+')
RELEASE_ASSET = re.compile(r'https://github\.com/[a-zA-Z0-9]([a-zA-Z0-9-]{0,37}[a-zA-Z0-9])?/[a-zA-Z0-9._-]+'
                           r'/releases/download/[^/]+/[^/]+')
# The commit a GitHub Actions run checked out, whose icon the entry links to.
COMMIT = re.compile(r'[0-9a-f]{40}')


def check_run():
    """In GitHub Actions, the run must belong to this repository: the
    registry would refuse the name to another owner anyway."""
    if os.environ.get('GITHUB_ACTIONS') != 'true':
        return
    owner = os.environ.get('GITHUB_REPOSITORY_OWNER', '')
    if owner != OWNER:
        fail(f'this run belongs to {owner!r}, but the name {NAME} is under io.github.{OWNER}/ '
             f'(the registry compares the owner case-sensitively)')
    if os.environ.get('GITHUB_REPOSITORY', '').lower() != REPOSITORY.lower():
        fail(f'this run belongs to {os.environ.get("GITHUB_REPOSITORY")}, not {REPOSITORY}')
    if os.environ.get('GITHUB_REPOSITORY_ID', REPOSITORY_ID) != REPOSITORY_ID:
        fail(f'the repository id is {os.environ["GITHUB_REPOSITORY_ID"]}, not {REPOSITORY_ID}')
    if not COMMIT.fullmatch(os.environ.get('GITHUB_SHA', '')):
        fail(f'GITHUB_SHA {os.environ.get("GITHUB_SHA")!r} is not a commit id')


def package(bundles, version, target):
    """The registry package of one target's bundle, which must be this
    version's bundle for that target."""
    path = bundles / bundle_name(version, target)
    if not path.is_file():
        fail(f'{path} is missing: build it with build_mcpb.py')
    with zipfile.ZipFile(path) as bundle:
        try:
            manifest = json.loads(bundle.read('manifest.json'))
        except KeyError:
            fail(f'{path.name} has no manifest.json')
        entry = manifest.get('server', {}).get('entry_point')
        if entry != f'server/{program_name(target)}' or entry not in bundle.namelist():
            fail(f'{path.name}: the entry point {entry!r} is not the bundle\'s program')
    if manifest.get('version') != version or manifest.get('name') != 'graphicle':
        fail(f'{path.name} is {manifest.get("name")} {manifest.get("version")}, not graphicle {version}')
    if manifest.get('compatibility', {}).get('platforms') != [TARGETS[target][0]]:
        fail(f'{path.name} is not for {TARGETS[target][0]} only')
    url = f'https://github.com/{REPOSITORY}/releases/download/v{version}/{path.name}'
    if not RELEASE_ASSET.fullmatch(url) or 'mcp' not in url.lower():
        fail(f'{url} is not a release asset address the registry accepts')
    return {
        'registryType': 'mcpb',
        'identifier': url,
        'version': version,
        'fileSha256': sha256(path),
        'transport': {'type': 'stdio'},
    }


def icons():
    """The entry's icon: the file every bundle holds, by its address in the
    run's commit, or on main outside GitHub Actions. The registry takes an
    HTTPS address of at most 255 characters."""
    icon()
    commit = os.environ['GITHUB_SHA'] if os.environ.get('GITHUB_ACTIONS') == 'true' else 'main'
    url = f'https://raw.githubusercontent.com/{REPOSITORY}/{commit}/{ICON}'
    if len(url) > 255:
        fail(f'{url} is longer than the registry allows')
    return [{'src': url, 'mimeType': 'image/png', 'sizes': [f'{ICON_SIZE}x{ICON_SIZE}']}]


def server_json(bundles, version):
    if not NAME_PATTERN.fullmatch(NAME) or len(NAME) > 200:
        fail(f'{NAME} is not a server name the registry accepts')
    if not 1 <= len(DESCRIPTION) <= 100 or not 1 <= len(TITLE) <= 100:
        fail('the registry allows a title and a description of 1 to 100 characters')
    return {
        '$schema': SCHEMA,
        'name': NAME,
        'title': TITLE,
        'description': DESCRIPTION,
        'version': version,
        'websiteUrl': f'https://github.com/{REPOSITORY}/blob/v{version}/docs/mcp.md',
        'icons': icons(),
        'repository': {'url': f'https://github.com/{REPOSITORY}', 'source': 'github', 'id': REPOSITORY_ID},
        'packages': [package(bundles, version, target) for target in TARGETS],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--version', required=True, help='the release version, without the tag\'s "v"')
    parser.add_argument('--bundles', type=Path, required=True, help='the folder holding the five bundles')
    parser.add_argument('--out', type=Path, help='where to write server.json (default: standard output)')
    args = parser.parse_args()
    if not VERSION.fullmatch(args.version):
        fail(f'--version {args.version!r} is not a version such as 0.1.2')
    check_run()
    text = json.dumps(server_json(args.bundles, args.version), indent=2, ensure_ascii=False) + '\n'
    if args.out:
        args.out.write_text(text, encoding='utf-8')
    else:
        sys.stdout.write(text)


if __name__ == '__main__':
    main()
