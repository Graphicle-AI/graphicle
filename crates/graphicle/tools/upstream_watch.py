#!/usr/bin/env python3
"""Routine check of the upstream files Graphicle adapted against each
project's latest default branch.

The provenance records (UPSTREAM.json: projects[].repository, commit,
selected_files[] and later_revisions[]) are merged by repository. The reference
for each recorded file is its newest record: a later_revisions entry naming the
path (newest committed_at wins), otherwise the pinned selected_files entry.
Every recorded file is then classified at the upstream HEAD:

  unchanged  same sha256 at HEAD
  changed    different bytes; a GitHub compare URL reference...HEAD and
             unified-diff line counts against the bytes at the recorded commit
  missing    no such file at HEAD (moved or deleted; an exact move is named)

The root license (LICENSE, LICENSE.md, LICENSE.txt or COPYING, whichever exists)
and any other retained text (licenses/<owner>__<name>__LICENSE-MIT.txt,
__NOTICE.txt, ...) are compared with the retained copies. A changed or removed
license is flagged prominently: it can change whether commercial use is allowed.

Access is read-only and minimal. `git ls-remote` resolves HEAD; a bare cache
repository receives `git fetch --depth 1 --filter=blob:none <url> <commit>` of
HEAD (and, for changed files, of the recorded commit), and `git show
<commit>:<path>` reads only the recorded files and license texts, so only those
blobs are downloaded. Commits behind come from a commits-only window
(--history-depth, no trees or blobs) and are omitted when not exact. Nothing is
checked out, installed or executed.

A small built-in blocklist names known-compromised refs, starting with the MemOS
tag v2.0.34. Blocked refs are never fetched; one still listed by `git ls-remote
--tags` is reported. A blocked HEAD or recorded commit, or a HEAD tree carrying
the compromise's own files, stops that project before any file is read.

Exit status: 0 nothing recorded changed; 1 a recorded file or license changed,
or a blocked ref is present; 2 errors (network, missing repository, blocked
HEAD, invalid records).
"""
import argparse
import datetime
import difflib
import fnmatch
import hashlib
import json
import os
import re
import subprocess
import sys
import time
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path, PurePosixPath

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT.parents[1]
RECORDS = [ROOT.parent / 'graphicle-memory/UPSTREAM.json', ROOT / 'UPSTREAM.json']
LICENSES = [ROOT.parent / 'graphicle-memory/licenses', ROOT / 'licenses']
OUTPUT = ROOT / 'test-artifacts/upstream-watch'
CONTRACT = 'graphicle.upstream-watch.v1'
REMOTE = 'https://github.com/{repository}.git'
LICENSE_NAMES = ['LICENSE', 'LICENSE.md', 'LICENSE.txt', 'COPYING']
PRIMARY_RETAINED = {'license', 'license.txt', 'license.md', 'copying', 'copying.txt', 'copying.md'}
LICENSE_LIKE = re.compile(r'(?i)(?:licen[cs]es?|copying|notice|patents?)(?:[-_.][A-Za-z0-9]+)*')
CODE_SUFFIXES = {'.py', '.js', '.mjs', '.ts', '.go', '.rs', '.sh', '.json', '.yml', '.yaml', '.toml', '.cfg', '.html'}
REPOSITORY = re.compile(r'[A-Za-z0-9][A-Za-z0-9._-]*/(?!\.\.?$)[A-Za-z0-9._-]+')
COMMIT = re.compile(r'[0-9a-f]{40}|[0-9a-f]{64}')
SHA256 = re.compile(r'[0-9a-f]{64}')
PREFIX = re.compile(r'[0-9a-f]{7,64}')
URL = re.compile(r'[A-Za-z][A-Za-z0-9+.-]*://|[^/]+@[^/]+:')
NOT_FOUND = re.compile(r'repository .*not found|does not appear to be a git repository|'
                       r'could not read username|authentication failed', re.I)
UNAVAILABLE = re.compile(r"not our ref|no such remote ref|couldn't find remote ref|unadvertised object", re.I)

# Known-compromised refs. `commits` are full ids or unambiguous prefixes;
# `ioc_paths` are the compromise's own files (fnmatch patterns): a HEAD tree
# carrying one is treated like a blocked HEAD, before any blob is read.
BLOCKLIST = [
    {'repository': 'MemTensor/MemOS', 'ref': 'refs/tags/v2.0.34',
     # The tag's commit and its parent, which added the payload; no branch contains either.
     'commits': ['41bf5c7fa89ee08ebedf3c662638b06fc29aca8d', 'b52958fdc9cdb6c81be90123bcf65c42be35b5b5'],
     'reported': 'SafeDep, 2026-09-23',
     'reason': 'supply-chain compromise: sckit credential stealer started on import and a CI '
               'publish-token hook (MemoryOS 2.0.34 on PyPI)',
     'ioc_paths': ['src/memos/.sckit/*', 'src/memos/_stage0*', 'src/memos/_pypi_bridge.sh',
                   '*sckit_poetry_build.py', '*_initial_ci_delivery.py']},
]

# Unattended and read-only: no stored credentials or prompts, no hooks, no
# background maintenance. Lazily fetched blobs inherit these settings.
GIT_CONFIG = ['credential.helper=', 'core.askPass=', 'core.hooksPath=/dev/null', 'gc.auto=0',
              'maintenance.auto=false', 'fetch.writeCommitGraph=false', 'protocol.version=2']
DROP_ENV = {'GIT_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE', 'GIT_OBJECT_DIRECTORY', 'GIT_COMMON_DIR',
            'GIT_ALTERNATE_OBJECT_DIRECTORIES', 'GIT_NAMESPACE', 'GIT_ASKPASS', 'SSH_ASKPASS', 'GIT_NO_LAZY_FETCH'}
HINTS = [('AGPL', r'GNU AFFERO GENERAL PUBLIC LICENSE'), ('LGPL', r'GNU LESSER GENERAL PUBLIC LICENSE'),
         ('GPL', r'GNU GENERAL PUBLIC LICENSE'), ('Apache-2.0', r'Apache License,?\s+(?:Version\s+)?2\.0'),
         ('MIT', r'MIT License|MIT Expat|Permission\s+is\s+hereby\s+granted,\s+free\s+of\s+charge'),
         ('BSD', r'Redistribution\s+and\s+use\s+in\s+source\s+and\s+binary\s+forms'),
         ('MPL-2.0', r'Mozilla Public License'), ('BUSL', r'Business Source License'),
         ('SSPL', r'Server Side Public License'), ('Elastic', r'Elastic License'), ('PolyForm', r'PolyForm'),
         ('Commons Clause', r'Commons Clause'), ('ee/ or enterprise terms', r'(?i)\benterprise\b|\bee/|"ee"')]
EXIT_TEXT = {0: 'Exit 0: every recorded file and license matches HEAD.',
             1: 'Exit 1: recorded files or licenses changed upstream, or a blocked ref is present.',
             2: 'Exit 2: errors; the check is incomplete.'}


class GitError(RuntimeError):
    def __init__(self, message, kind='git', stderr=''):
        super().__init__(message)
        self.kind, self.stderr = kind, stderr


class Blocked(RuntimeError):
    """A known-compromised commit or tree; the project stops before anything is read."""


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def display(path):
    path = Path(path).resolve()
    try:
        return str(path.relative_to(PROJECT))
    except ValueError:
        return str(path)


def unique(items):
    return list(dict.fromkeys(items))


def run_git(args, repo=None, network=False, stdin=None, timeout=120, config=()):
    command = ['git']
    for item in (*GIT_CONFIG, *config):
        command += ['-c', item]
    if repo is not None:
        command.append(f'--git-dir={repo}')
    env = {key: value for key, value in os.environ.items() if key not in DROP_ENV}
    env.update(GIT_TERMINAL_PROMPT='0', LC_ALL='C', LANGUAGE='C')
    if not network:
        env['GIT_NO_LAZY_FETCH'] = '1'  # local reads never reach the network
    feed = {'input': stdin} if stdin is not None else {'stdin': subprocess.DEVNULL}
    try:
        return subprocess.run(command + args, capture_output=True, env=env, timeout=timeout, **feed)
    except subprocess.TimeoutExpired:
        raise GitError(f'git {args[0]} timed out after {timeout} s', 'network' if network else 'git') from None


def git(args, repo=None, network=False, stdin=None, timeout=120, config=()):
    done = run_git(args, repo, network, stdin, timeout, config)
    if done.returncode:
        stderr = done.stderr.decode('utf-8', 'replace').strip()
        kind = 'missing_repository' if NOT_FOUND.search(stderr) else 'network' if network else 'git'
        raise GitError(f"git {args[0]}: {stderr[-500:] or f'exit {done.returncode}'}", kind, stderr)
    return done.stdout


# Records ---------------------------------------------------------------------

def checked_commit(value, context):
    if not isinstance(value, str) or not COMMIT.fullmatch(value):
        raise ValueError(f'{context}: commit must be a full hexadecimal id, not {value!r}')
    return value


def checked_path(value, context):
    pure = PurePosixPath(value) if isinstance(value, str) else None
    if (pure is None or not value or pure.as_posix() != value or pure.is_absolute() or '..' in pure.parts
            or any(c in value for c in '\0\n\r')):
        raise ValueError(f'{context}: path must be a normalized relative path, not {value!r}')
    return value


def timestamp(value):
    try:
        moment = datetime.datetime.fromisoformat(value.replace('Z', '+00:00'))
    except (AttributeError, ValueError):
        return float('-inf')
    return (moment if moment.tzinfo else moment.replace(tzinfo=datetime.timezone.utc)).timestamp()


def add_files(project, files, commit, source, committed_at, rank, where, context):
    if not isinstance(files, list):
        raise ValueError(f'{context}: selected_files must be a list')
    for number, item in enumerate(files):
        label = f'{context} selected_files[{number}]'
        if not isinstance(item, dict):
            raise ValueError(f'{label}: expected an object')
        path = checked_path(item.get('path'), label)
        digest = item.get('sha256')
        if not isinstance(digest, str) or not SHA256.fullmatch(digest):
            raise ValueError(f'{label}: sha256 must be 64 lowercase hexadecimal digits')
        project['candidates'].setdefault(path, []).append(
            {'commit': commit, 'sha256': digest, 'source': source, 'committed_at': committed_at,
             'record': where, 'rank': rank})


def choose(project):
    """Pick each path's newest record: later_revisions over selected_files, then the
    latest committed_at, then the record and entry listed last."""
    references, warnings = {}, []
    for path, candidates in sorted(project.pop('candidates').items()):
        best = max(candidates, key=lambda c: c['rank'])
        same = [c for c in candidates if c['commit'] == best['commit']]
        if len({c['sha256'] for c in same}) > 1:
            warnings.append(f'{path}: records disagree about its sha256 at {best["commit"][:12]}; '
                            f'using {best["record"]}')
        reference = {'commit': best['commit'], 'sha256': best['sha256'], 'source': best['source'],
                     'committed_at': best['committed_at'],
                     'records': unique(c['record'] for c in same if c['sha256'] == best['sha256'])}
        older = unique((c['commit'], c['sha256'], c['source']) for c in candidates if c['commit'] != best['commit'])
        if older:
            reference['superseded'] = [{'commit': c, 'sha256': s, 'source': src} for c, s, src in older]
        references[path] = reference
    project['references'], project['warnings'] = references, warnings


def load_records(paths):
    """Merge the provenance records by repository. Only repository, commit,
    selected_files and later_revisions are read."""
    projects = {}
    for order, path in enumerate(paths):
        where = display(path)
        document = json.loads(Path(path).read_text())
        entries = document.get('projects') if isinstance(document, dict) else None
        if not isinstance(entries, list):
            raise ValueError(f'{where}: no projects[]')
        for index, entry in enumerate(entries):
            context = f'{where} projects[{index}]'
            repository = entry.get('repository') if isinstance(entry, dict) else None
            if not isinstance(repository, str) or not REPOSITORY.fullmatch(repository):
                raise ValueError(f'{context}: repository must be OWNER/NAME, not {repository!r}')
            pin = checked_commit(entry.get('commit'), context)
            project = projects.setdefault(repository.lower(), {'repository': repository, 'pins': [],
                                                               'records': [], 'candidates': {}})
            project['pins'] = unique([*project['pins'], pin])
            project['records'] = unique([*project['records'], where])
            add_files(project, entry.get('selected_files', []), pin, 'selected_files', None,
                      (0, float('-inf'), order, 0), where, context)
            revisions = entry.get('later_revisions', [])
            if not isinstance(revisions, list):
                raise ValueError(f'{context}: later_revisions must be a list')
            for number, revision in enumerate(revisions):
                label = f'{context} later_revisions[{number}]'
                if not isinstance(revision, dict):
                    raise ValueError(f'{label}: expected an object')
                commit = checked_commit(revision.get('commit'), label)
                add_files(project, revision.get('selected_files', []), commit, 'later_revisions',
                          revision.get('committed_at'), (1, timestamp(revision.get('committed_at')), order, number),
                          where, label)
    for project in projects.values():
        choose(project)
    return sorted(projects.values(), key=lambda p: p['repository'].lower())


# Remotes and the blocklist ------------------------------------------------------

def remote_for(repository, pattern, mapping):
    owner, name = repository.split('/')
    value = mapping.get(repository.lower())
    if value is None:
        value = pattern.format(repository=repository, owner=owner, name=name, slug=f'{owner}__{name}')
    if value.startswith('-'):
        raise ValueError(f'remote for {repository} must not start with "-": {value!r}')
    if not URL.match(value):
        # A local repository. It must be named by URL: a plain path cannot name the
        # promisor remote that serves lazily fetched blobs.
        value = 'file://' + str(Path(value).expanduser().resolve())
    return value


def load_map(value):
    if not value:
        return {}
    mapping = json.loads(value if value.lstrip().startswith('{') else Path(value).read_text())
    if not isinstance(mapping, dict) or not all(isinstance(k, str) and isinstance(v, str) for k, v in mapping.items()):
        raise ValueError('--remote-map must be a JSON object of OWNER/NAME to URL or path')
    return {key.lower(): url for key, url in mapping.items()}


def parse_block(value):
    repository, separator, ref = value.rpartition('@')
    if not separator or not REPOSITORY.fullmatch(repository) or not ref:
        raise ValueError(f'--block expects OWNER/NAME@TAG_OR_COMMIT, not {value!r}')
    entry = {'repository': repository, 'reported': 'command line', 'reason': 'blocked with --block', 'ioc_paths': []}
    if PREFIX.fullmatch(ref.lower()):
        entry['commits'] = [ref.lower()]
    else:
        entry['ref'] = ref if ref.startswith('refs/') else f'refs/tags/{ref}'
    return entry


def blocked_commit(commit, entries):
    return next((e for e in entries for prefix in e.get('commits', ()) if commit.lower().startswith(prefix)), None)


def describe(entry):
    return f"{entry.get('ref') or 'blocked commit'}, {entry['reported']}: {entry['reason']}"


def blocked_tags(url, entries, timeout):
    """Blocked tags still listed by the remote, by name or by the commit they point at."""
    found = {}
    for line in git(['ls-remote', '--tags', url], network=True, timeout=timeout).decode().splitlines():
        oid, _, ref = line.partition('\t')
        name, peeled = ref.removesuffix('^{}'), ref.endswith('^{}')
        for entry in entries:
            commit = any(oid.startswith(prefix) for prefix in entry.get('commits', ()))
            if name == entry.get('ref') or commit:
                item = found.setdefault(name, {'ref': name, 'object': oid, 'blocked_commit': False,
                                               'reported': entry['reported'], 'reason': entry['reason']})
                if peeled:
                    item['object'] = oid  # the commit an annotated tag points at
                item['blocked_commit'] |= commit
    return list(found.values())


# Git object access -----------------------------------------------------------------

def resolve_head(url, timeout):
    head = branch = None
    for line in git(['ls-remote', '--symref', url, 'HEAD'], network=True, timeout=timeout).decode().splitlines():
        if line.startswith('ref: ') and line.endswith('\tHEAD'):
            branch = line[5:-5].removeprefix('refs/heads/')
        elif line.endswith('\tHEAD'):
            head = line.split('\t')[0]
    if not head or not COMMIT.fullmatch(head):
        raise GitError(f'{url} has no HEAD commit', 'missing_repository')
    return head, branch


def cache_repo(cache, repository, url):
    repo = cache / f"{repository.replace('/', '__')}-{sha256(url.encode())[:12]}.git"
    if not (repo / 'objects').is_dir():
        repo.mkdir(parents=True, exist_ok=True)
        git(['init', '--bare', '--quiet', '--template=', str(repo)])
    return repo


def fetch(repo, url, commit, entries, timeout, depth=1, spec='blob:none'):
    hit = blocked_commit(commit, entries)
    if hit:  # checked before every fetch; blocked refs are never fetched
        raise Blocked(f'refusing to fetch blocked commit {commit} ({describe(hit)})')
    git(['fetch', '--quiet', '--no-tags', '--no-write-fetch-head', '--no-recurse-submodules',
         f'--depth={depth}', f'--filter={spec}', url, commit], repo, network=True, timeout=timeout)


def list_tree(repo, commit, paths=None):
    args = ['ls-tree', '-z', '--full-tree'] + (['-r', commit] if paths is None else [commit, '--', *paths])
    entries = {}
    for item in git(args, repo).split(b'\0'):
        if item:
            meta, _, name = item.partition(b'\t')
            mode, kind, oid = meta.decode().split()
            entries[name.decode('utf-8', 'surrogateescape')] = {'mode': mode, 'type': kind, 'oid': oid}
    return entries


def tree_at(repo, url, commit, entries, timeout, paths=None):
    """The commit's tree listing; fetches the commit and its trees (no blobs) unless cached."""
    try:
        return list_tree(repo, commit, paths)
    except GitError:
        fetch(repo, url, commit, entries, timeout)
        return list_tree(repo, commit, paths)


def present(repo, names):
    if not names:
        return set()
    done = run_git(['cat-file', '--batch-check=%(objectname) %(objecttype)'], repo,
                   stdin=''.join(f'{name}\n' for name in names).encode())
    return {line.split()[0] for line in done.stdout.decode().splitlines()
            if not line.endswith((' missing', ' ambiguous'))}


def download(repo, url, oids, timeout):
    """Fetch the named blobs in one request, as git's own lazy fetch does per object."""
    wanted = sorted(set(oids) - present(repo, oids))
    if wanted:
        git(['fetch', '--quiet', '--no-tags', '--no-write-fetch-head', '--no-recurse-submodules',
             '--filter=blob:none', '--stdin', url], repo, network=True, timeout=timeout,
            stdin=''.join(f'{oid}\n' for oid in wanted).encode(), config=['fetch.negotiationAlgorithm=noop'])


def show(repo, commit, path, timeout):
    # git show prints a blob's bytes unchanged (no textconv unless asked); a blob
    # the batch download missed is fetched lazily, and only that blob.
    return git(['show', f'{commit}:{path}'], repo, network=True, timeout=timeout)


def commit_time(repo, commit):
    done = run_git(['cat-file', 'commit', commit], repo)
    for line in done.stdout.decode('utf-8', 'replace').splitlines() if done.returncode == 0 else ():
        if not line:
            break
        match = re.fullmatch(r'committer .* (\d+) ([+-])(\d\d)(\d\d)', line)
        if match:
            seconds, sign, hours, minutes = match.groups()
            offset = datetime.timedelta(hours=int(hours), minutes=int(minutes)) * (1 if sign == '+' else -1)
            return datetime.datetime.fromtimestamp(int(seconds), datetime.timezone(offset)).isoformat()
    return None


def history(repo, url, head, commits, depth, entries, timeout):
    """Commits behind HEAD from a commits-only window of HEAD's last `depth` commits.
    A count is given only when exact: the recorded commit is an ancestor inside the
    window and no counted commit sits on the window's cut."""
    fetch(repo, url, head, entries, timeout, depth=depth, spec='tree:0')
    shallow = repo / 'shallow'
    cut = set(shallow.read_text().split()) if shallow.exists() else set()
    have = present(repo, commits)
    counts = {}
    for commit in commits:
        if commit == head:
            counts[commit] = (0, None)
        elif commit not in have or run_git(['merge-base', '--is-ancestor', commit, head], repo).returncode:
            counts[commit] = (None, f'not reachable from HEAD within its last {depth} commits')
        else:
            between = git(['rev-list', f'{commit}..{head}'], repo).decode().split()
            counts[commit] = ((None, f'counting exactly needs history older than the last {depth} commits')
                              if cut.intersection(between) else (len(between), None))
    return counts


# Comparison ------------------------------------------------------------------------

def diff(old, new, path):
    if b'\0' in old or b'\0' in new:
        return {'binary': True}, ''
    lines = list(difflib.unified_diff(old.decode('utf-8', 'replace').splitlines(keepends=True),
                                      new.decode('utf-8', 'replace').splitlines(keepends=True),
                                      f'a/{path}', f'b/{path}'))
    body = [line for line in lines[2:] if not line.startswith('@@')]
    counts = {'added': sum(line.startswith('+') for line in body),
              'removed': sum(line.startswith('-') for line in body), 'unified_lines': len(lines)}
    return counts, ''.join(line if line.endswith('\n') else line + '\n\\ No newline at end of file\n' for line in lines)


def write_diff(directory, repository, name, text):
    target = directory / repository.replace('/', '__') / f'{name}.diff'
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(text)
    return display(target)


def license_hint(data):
    """Keyword hint for a reader, not a license determination."""
    text = data.decode('utf-8', 'replace')
    return ' + '.join(name for name, pattern in HINTS if re.search(pattern, text)) or 'unrecognized'


def license_items(repository, tree, directories):
    """Pair each retained text (<owner>__<name>__<file>, `.txt` added to names
    without an extension) with its file at the HEAD root. License-like root files
    without a retained copy are listed too."""
    prefix = repository.replace('/', '__').lower() + '__'
    root = {name.lower(): name for name, entry in tree.items() if '/' not in name and entry['type'] == 'blob'}

    def upstream(names):
        return next((root[name.lower()] for name in names if name.lower() in root), None)

    primary, additional = [], {}
    for directory in directories:
        for path in sorted(directory.iterdir()) if directory.is_dir() else ():
            if not path.is_file() or not path.name.lower().startswith(prefix):
                continue
            suffix = path.name[len(prefix):]
            stem = suffix[:-4] if suffix.lower().endswith('.txt') else suffix
            if suffix.lower() in PRIMARY_RETAINED:
                primary.append((path, [stem, suffix]))
            else:
                additional.setdefault(stem.lower(), {'kind': 'additional', 'head_file': upstream([stem, suffix]),
                                                     'retained': []})['retained'].append(path)
    # The retained copy's own name first, then whichever of LICENSE_NAMES exists.
    names = [name for _, own in primary for name in own] + LICENSE_NAMES
    items = [{'kind': 'license', 'head_file': upstream(names), 'retained': [path for path, _ in primary]},
             *additional.values()]
    covered = {item['head_file'] for item in items}
    items += [{'kind': 'not_retained', 'head_file': name, 'retained': []} for name in sorted(root.values())
              if LICENSE_LIKE.fullmatch(name) and PurePosixPath(name).suffix.lower() not in CODE_SUFFIXES
              and name not in covered]
    return items


def compare_url(repository, base, head):
    return f'https://github.com/{repository}/compare/{base}...{head}'


def inspect(project, a, result):
    repository = project['repository']
    entries = [e for e in a.blocklist if e['repository'].lower() == repository.lower()]
    url = result['url'] = remote_for(repository, a.remote, a.mapping)
    head, result['default_branch'] = resolve_head(url, a.timeout)
    result['head'] = head
    if any(e.get('ref') or e.get('commits') for e in entries):
        result['blocked_refs'] = blocked_tags(url, entries, a.timeout)
    commits = unique([*project['pins'], *(r['commit'] for r in project['references'].values())])
    for commit, role in [(head, 'HEAD'), *((c, 'recorded commit') for c in commits)]:
        hit = blocked_commit(commit, entries)
        if hit:
            raise Blocked(f'{role} {commit} is blocked ({describe(hit)}); nothing was fetched')
    repo = cache_repo(a.cache, repository, url)
    tree = tree_at(repo, url, head, entries, a.timeout)
    result['head_committed_at'] = commit_time(repo, head)
    iocs = sorted(path for path in tree for e in entries for pattern in e.get('ioc_paths', ())
                  if fnmatch.fnmatchcase(path, pattern))
    if iocs:
        raise Blocked(f'HEAD {head} carries files of a known compromise ({", ".join(iocs[:4])}); no file was read')

    licenses = license_items(repository, tree, a.licenses)
    wanted = [path for path in project['references'] if tree.get(path, {}).get('type') == 'blob']
    wanted = unique(wanted + [item['head_file'] for item in licenses if item['head_file']])
    download(repo, url, [tree[path]['oid'] for path in wanted], a.timeout)
    contents = {path: show(repo, head, path, a.timeout) for path in wanted}

    files = result['files']
    for path, reference in project['references'].items():
        item = {'path': path, 'status': 'missing', 'reference': reference}
        if path in contents:
            item['head_sha256'] = sha256(contents[path])
            item['status'] = 'unchanged' if item['head_sha256'] == reference['sha256'] else 'changed'
        elif path in tree or any(p.startswith(path + '/') for p in tree):
            item['head_type'] = tree[path]['type'] if path in tree else 'tree'
        if item['status'] != 'unchanged':
            item['compare_url'] = compare_url(repository, reference['commit'], head)
        files.append(item)

    # Changed and missing files: read the recorded bytes for a diff, check them
    # against the recorded sha256 and look for an exact move.
    pending = {}
    for item in files:
        if item['status'] != 'unchanged':
            pending.setdefault(item['reference']['commit'], []).append(item)
    for commit, group in pending.items():
        try:
            old = tree_at(repo, url, commit, entries, a.timeout, [item['path'] for item in group])
        except GitError as error:
            if not UNAVAILABLE.search(error.stderr):
                raise
            result['warnings'].append(f'recorded commit {commit[:12]} is not available upstream; '
                                      f'{len(group)} files not compared with it')
            continue
        blobs = {path: entry for path, entry in old.items() if entry['type'] == 'blob'}
        download(repo, url, [entry['oid'] for entry in blobs.values()], a.timeout)
        for item in group:
            path = item['path']
            if path not in blobs:
                item['reference_verified'] = False
                result['warnings'].append(f'{path} is not a file at its recorded commit {commit[:12]}')
                continue
            before = show(repo, commit, path, a.timeout)
            item['reference_verified'] = sha256(before) == item['reference']['sha256']
            if not item['reference_verified']:
                result['warnings'].append(f'{path}: bytes at {commit[:12]} differ from the recorded sha256')
            if item['status'] == 'changed':
                item['diff'], text = diff(before, contents[path], path)
                if a.diff_dir and text:
                    item['diff_file'] = write_diff(a.diff_dir, repository, path, text)
            else:
                name = PurePosixPath(path).name
                item['moved_to'] = sorted(p for p, e in tree.items() if e['oid'] == blobs[path]['oid'])
                item['same_name_at_head'] = sorted(p for p in tree if PurePosixPath(p).name == name
                                                   and p not in item['moved_to'])[:10]

    for item in licenses:
        head_file = item['head_file']
        data = contents.get(head_file) if head_file else None
        copies = [(path, path.read_bytes()) for path in item['retained']]
        digest = sha256(data) if data is not None else None
        if data is None:
            status = 'missing' if copies else 'absent'
        elif not copies:
            status = 'no_retained_copy'
        else:
            status = 'unchanged' if all(sha256(blob) == digest for _, blob in copies) else 'changed'
        entry = {'kind': item['kind'], 'status': status, 'head_file': head_file, 'head_sha256': digest,
                 'head_hint': license_hint(data) if data is not None else None,
                 'retained': [{'path': display(path), 'sha256': sha256(blob), 'hint': license_hint(blob),
                               'matches_head': sha256(blob) == digest} for path, blob in copies]}
        if status == 'changed':
            blob = next(blob for _, blob in copies if sha256(blob) != digest)
            entry['diff'], text = diff(blob, data, head_file)
            if a.diff_dir and text:
                entry['diff_file'] = write_diff(a.diff_dir, repository, head_file, text)
        if status in ('changed', 'missing'):
            entry['compare_url'] = compare_url(repository, project['pins'][0], head)
        elif status == 'absent':
            result['warnings'].append('no license file at the HEAD root and no retained copy')
        elif status == 'no_retained_copy':
            result['warnings'].append(f'no retained copy of {head_file} to compare with')
        result['licenses'].append(entry)

    counts = {}
    if a.history_depth:
        try:
            counts = history(repo, url, head, commits, a.history_depth, entries, a.timeout)
        except GitError as error:
            result['warnings'].append(f'commits behind not computed: {error}')
        # The window holds commit objects only; a blocked commit there means it
        # is in the default branch's history.
        found = present(repo, [c for e in entries for c in e.get('commits', ())])
        for entry in entries:
            for prefix in entry.get('commits', ()):
                if any(oid.startswith(prefix) for oid in found):
                    result['blocked_refs'].append({'ref': 'HEAD history', 'object': prefix, 'blocked_commit': True,
                                                   'reported': entry['reported'], 'reason': entry['reason']})
    roles = {c: 'pin' for c in project['pins']}
    for reference in project['references'].values():
        roles.setdefault(reference['commit'], 'later_revision')
    recorded = {r['commit']: r['committed_at'] for r in project['references'].values() if r['committed_at']}
    for commit in commits:
        behind, note = counts.get(commit, (0, None) if commit == head else
                                  (None, 'skipped' if not a.history_depth else 'unknown'))
        result['references'].append({'commit': commit, 'role': roles[commit],
                                     'committed_at': commit_time(repo, commit) or recorded.get(commit),
                                     'commits_behind': behind, 'note': note,
                                     'compare_url': compare_url(repository, commit, head)})
    result['commits_behind'] = result['references'][0]['commits_behind'] if result['references'] else None


def check(project, a):
    started = time.monotonic()
    result = {'repository': project['repository'], 'records': project['records'], 'url': None,
              'default_branch': None, 'head': None, 'head_committed_at': None, 'pinned_commit': project['pins'][0],
              'commits_behind': None, 'status': None, 'files': [], 'licenses': [], 'references': [],
              'blocked_refs': [], 'warnings': list(project['warnings']), 'error': None}
    if len(project['pins']) > 1:
        result['warnings'].append(f'records pin different commits: {", ".join(c[:12] for c in project["pins"])}')
    try:
        inspect(project, a, result)
    except Blocked as error:
        result['error'] = {'kind': 'blocked', 'message': str(error)}
    except GitError as error:
        result['error'] = {'kind': error.kind, 'message': str(error)}
    except Exception as error:  # one broken project must not hide the others
        result['error'] = {'kind': 'error', 'message': f'{type(error).__name__}: {error}'}
    flagged = any(f['status'] != 'unchanged' for f in result['files']) or \
        any(item['status'] in ('changed', 'missing') for item in result['licenses'])
    result['status'] = ('blocked' if result['error'] and result['error']['kind'] == 'blocked' else
                        'error' if result['error'] else 'changed' if flagged else 'unchanged')
    result['seconds'] = round(time.monotonic() - started, 1)
    sys.stderr.write(f"checked {project['repository']}: {result['status']} ({result['seconds']} s)\n")
    return result


# Report --------------------------------------------------------------------------------

def build_report(results, a, records, started):
    files = Counter(item['status'] for p in results for item in p['files'])
    flagged = [(p, item) for p in results for item in p['licenses'] if item['status'] in ('changed', 'missing')]
    blocked = [(p, ref) for p in results for ref in p['blocked_refs']]
    errors = [p for p in results if p['error']]
    alerts = [f"BLOCKED: {p['repository']}: {p['error']['message']}" for p in errors if p['error']['kind'] == 'blocked']
    for p, item in flagged:
        name = item['head_file'] or (item['retained'][0]['path'] if item['retained'] else 'license')
        was = ', '.join(unique(copy['hint'] for copy in item['retained'])) or 'no retained copy'
        now = item['head_hint'] or 'removed at HEAD'
        alerts.append(f"LICENSE {item['status'].upper()}: {p['repository']} {name}: {was} -> {now}. "
                      f"Check that commercial use is still allowed before porting anything newer.")
    for p, ref in blocked:
        alerts.append(f"BLOCKED REF PRESENT: {p['repository']} {ref['ref']} -> {ref['object'][:12]} "
                      f"({ref['reported']}: {ref['reason']}). Never fetch, check out or install it.")
    code = 2 if errors else 1 if (files['changed'] or files['missing'] or flagged or blocked) else 0
    return {
        'contract': CONTRACT,
        'created_at': datetime.datetime.now(datetime.timezone.utc).isoformat(timespec='seconds'),
        'seconds': round(time.monotonic() - started, 1),
        'tool_sha256': sha256(Path(__file__).read_bytes()),
        'git': git(['--version']).decode().strip(),
        'records': [{'path': display(path), 'sha256': sha256(Path(path).read_bytes())} for path in records],
        'licenses': [display(directory) for directory in a.licenses],
        'remote': a.remote, 'remote_map': a.mapping, 'history_depth': a.history_depth, 'blocklist': a.blocklist,
        'scope': 'Read-only: ls-remote, shallow partial fetches of HEAD and of recorded commits with changed '
                 'files, a commits-only history window, and the blobs of recorded files and license texts. '
                 'Nothing was checked out, installed or executed.',
        'exit_code': code,
        'summary': {'projects': len(results), 'errors': len(errors),
                    'files': {'total': sum(files.values()), **{k: files[k] for k in ('unchanged', 'changed', 'missing')}},
                    'license_changes': len(flagged), 'blocked_refs_present': len(blocked),
                    'warnings': sum(len(p['warnings']) for p in results)},
        'alerts': alerts,
        'projects': results,
    }


def summarize(report, out):
    s, f = report['summary'], report['summary']['files']
    lines = [f"Upstream watch {report['created_at']}: {s['projects']} projects, {f['total']} recorded files: "
             f"{f['unchanged']} unchanged, {f['changed']} changed, {f['missing']} missing at HEAD; "
             f"license changes: {s['license_changes']}; blocked refs present: {s['blocked_refs_present']}; "
             f"errors: {s['errors']}"]
    lines += [f'!! {alert}' for alert in report['alerts']]
    for p in report['projects']:
        counts = Counter(item['status'] for item in p['files'])
        state = ', '.join(f'{counts[k]} {k}' for k in ('unchanged', 'changed', 'missing') if counts[k])
        head = f"HEAD {p['head'][:12]} ({p['default_branch'] or 'detached'})" if p['head'] else 'HEAD unknown'
        behind = f", pin +{p['commits_behind']} commits" if p['commits_behind'] is not None else ''
        lines.append(f"{p['repository']}: {head}{behind}: {state or 'no files read'}")
        if p['error']:
            lines.append(f"  ERROR ({p['error']['kind']}): {p['error']['message']}")
        for item in p['files']:
            if item['status'] == 'changed':
                d = item.get('diff', {})
                lines.append(f"  changed  {item['path']}" + (f"  +{d['added']} -{d['removed']}" if 'added' in d else ''))
            elif item['status'] == 'missing':
                moved = item.get('moved_to')
                lines.append(f"  missing  {item['path']}" + (f"  (moved to {', '.join(moved)})" if moved else ''))
        changed = {item['reference']['commit'] for item in p['files'] if item['status'] != 'unchanged'}
        lines += [f"  compare  {ref['compare_url']}" for ref in p['references'] if ref['commit'] in changed]
        lines += [f'  warning  {warning}' for warning in p['warnings']]
    lines += [f'Report: {display(out)}', EXIT_TEXT[report['exit_code']]]
    return '\n'.join(lines)


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('--records', type=Path, action='append',
                   help='UPSTREAM.json provenance record (repeatable; default: graphicle-memory and graphicle)')
    p.add_argument('--licenses', type=Path, action='append',
                   help='directory of retained license texts (repeatable; default: both licenses/ directories)')
    p.add_argument('--remote', default=REMOTE,
                   help='URL or local path pattern with {repository}, {owner}, {name} or {slug} (owner__name)')
    p.add_argument('--remote-map', help='JSON object (inline or a file) of OWNER/NAME to URL or local path; '
                                        'overrides --remote')
    p.add_argument('--out', type=Path, help='JSON report (default: test-artifacts/upstream-watch/upstream-watch-DATE.json)')
    p.add_argument('--cache', type=Path, default=OUTPUT / 'cache',
                   help='scratch directory for the bare partial repositories (default: %(default)s)')
    p.add_argument('--diff-dir', type=Path, help='also write unified diffs of changed files and licenses here')
    p.add_argument('--only', action='append', help='check only this OWNER/NAME (repeatable)')
    p.add_argument('--block', action='append', default=[],
                   help='also treat OWNER/NAME@TAG_OR_COMMIT as compromised (repeatable)')
    p.add_argument('--history-depth', type=int, default=1000,
                   help='commits-only window used to count commits behind (0: skip)')
    p.add_argument('--jobs', type=int, default=4)
    p.add_argument('--timeout', type=int, default=300, help='seconds per network git command')
    a = p.parse_args()
    if a.history_depth < 0 or a.jobs < 1 or a.timeout < 1:
        p.error('--history-depth must be >= 0, --jobs and --timeout >= 1')
    try:
        a.remote.format(repository='o/n', owner='o', name='n', slug='o__n')
    except (KeyError, IndexError, ValueError) as error:
        p.error(f'--remote: {error!r}')
    started = time.monotonic()
    records = a.records or RECORDS
    a.licenses = a.licenses or LICENSES
    try:
        missing = [str(directory) for directory in a.licenses if not directory.is_dir()]
        if missing:
            raise ValueError(f'not a licenses directory: {", ".join(missing)}')
        projects = load_records(records)
        a.mapping = load_map(a.remote_map)
        a.blocklist = BLOCKLIST + [parse_block(value) for value in a.block]
    except (OSError, ValueError) as error:
        print(f'upstream watch: {error}', file=sys.stderr)
        return 2
    if a.only:
        wanted = {name.lower() for name in a.only}
        unknown = wanted - {project['repository'].lower() for project in projects}
        if unknown:
            print(f'upstream watch: not in the records: {", ".join(sorted(unknown))}', file=sys.stderr)
            return 2
        projects = [project for project in projects if project['repository'].lower() in wanted]
    today = datetime.datetime.now(datetime.timezone.utc).date().isoformat()
    out = (a.out or OUTPUT / f'upstream-watch-{today}.json').resolve()
    a.cache = a.cache.resolve()
    a.cache.mkdir(parents=True, exist_ok=True)
    a.diff_dir = a.diff_dir.resolve() if a.diff_dir else None
    with ThreadPoolExecutor(max_workers=a.jobs) as pool:
        results = list(pool.map(lambda project: check(project, a), projects))
    report = build_report(results, a, records, started)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(report, indent=2) + '\n')
    print(summarize(report, out))
    return report['exit_code']


if __name__ == '__main__':
    sys.exit(main())
