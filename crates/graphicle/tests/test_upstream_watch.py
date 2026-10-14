"""Tests of tools/upstream_watch.py against local git repositories standing in
for GitHub: recorded-file classification, later_revisions references, license
and blocklist checks and the exit codes. No network."""
import hashlib
import json
import os
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
TOOL = ROOT / 'tools/upstream_watch.py'
sys.path.insert(0, str(ROOT / 'tools'))
import upstream_watch  # noqa: E402

MIT = ('MIT License\n\nCopyright (c) 2026 Example\n\nPermission is hereby granted, free of charge, to any person '
       'obtaining a copy of this software, to deal in the Software without restriction.\n')
BUSL = 'Business Source License 1.1\n\nLicensor: Example\nProduction use requires a separate agreement.\n'


def sha(text):
    return hashlib.sha256(text.encode()).hexdigest()


@pytest.fixture
def env(tmp_path):
    """Git isolated from the user's configuration."""
    (tmp_path / 'gitconfig').write_text('')
    values = {key: value for key, value in os.environ.items() if not key.startswith('GIT_')}
    values.update(HOME=str(tmp_path), GIT_CONFIG_GLOBAL=str(tmp_path / 'gitconfig'), GIT_CONFIG_NOSYSTEM='1',
                  GIT_AUTHOR_NAME='Upstream', GIT_AUTHOR_EMAIL='upstream@example.invalid',
                  GIT_COMMITTER_NAME='Upstream', GIT_COMMITTER_EMAIL='upstream@example.invalid')
    return values


@pytest.fixture
def licenses(tmp_path):
    directory = tmp_path / 'licenses'
    directory.mkdir()
    return directory


class Upstream:
    """A local repository standing in for a GitHub project."""

    def __init__(self, path, env):
        self.path, self.env = path, env
        path.mkdir(parents=True)
        self.git('init', '-q', '-b', 'main')
        self.git('config', 'uploadpack.allowFilter', 'true')  # serve partial fetches, as GitHub does

    def git(self, *args):
        return subprocess.run(['git', '-C', str(self.path), *args], env=self.env, capture_output=True,
                              text=True, check=True).stdout.strip()

    def commit(self, files):
        for name, content in files.items():
            if content is None:
                self.git('rm', '-q', name)
            else:
                (self.path / name).parent.mkdir(parents=True, exist_ok=True)
                (self.path / name).write_text(content)
        self.git('add', '-A')
        self.git('commit', '-q', '-m', 'change')
        return self.git('rev-parse', 'HEAD')


def selected(files):
    return [{'path': path, 'sha256': sha(text), 'inspection': 'test'} for path, text in files.items()]


def write_record(path, *projects):
    path.write_text(json.dumps({'schema': 'test', 'projects': list(projects)}, indent=2))
    return path


def watch(tmp_path, env, *args):
    out = tmp_path / 'report.json'
    done = subprocess.run([sys.executable, str(TOOL), '--out', str(out), '--cache', str(tmp_path / 'cache'), *args],
                          env=env, capture_output=True, text=True, timeout=300)
    return done, (json.loads(out.read_text()) if out.exists() else None)


def cached(tmp_path, env, repository, oid):
    """Whether the watch's cache holds an object, checked without fetching it."""
    repo, = (tmp_path / 'cache').glob(repository.replace('/', '__') + '-*.git')
    return subprocess.run(['git', '--git-dir', str(repo), 'cat-file', '-e', oid], capture_output=True,
                          env=dict(env, GIT_NO_LAZY_FETCH='1')).returncode == 0


def test_classifies_recorded_files_and_prefers_later_revisions(tmp_path, env, licenses):
    up = Upstream(tmp_path / 'remotes/acme__widgets', env)
    first = {'pkg/same.py': 'same\n', 'pkg/edit.py': 'one\ntwo\n', 'pkg/gone.py': 'gone\n',
             'pkg/moved.py': 'moved\n', 'pkg/revised.py': 'first\n'}
    pin = up.commit({**first, 'LICENSE': MIT, 'data/large.bin': 'x' * 200_000})
    revision = up.commit({'pkg/revised.py': 'second\n'})
    head = up.commit({'pkg/edit.py': 'one\ntwo\nthree\n', 'pkg/gone.py': None, 'pkg/moved.py': None,
                      'lib/moved.py': 'moved\n'})
    (licenses / 'acme__widgets__LICENSE.txt').write_text(MIT)
    # Two records of one project, like graphicle-memory and graphicle; the
    # second re-reviewed pkg/revised.py at a later commit.
    core = write_record(tmp_path / 'core.json', {
        'repository': 'acme/widgets', 'commit': pin,
        'selected_files': selected({'pkg/same.py': 'same\n', 'pkg/revised.py': 'first\n'})})
    service = write_record(tmp_path / 'service.json', {
        'repository': 'acme/widgets', 'commit': pin, 'selected_files': selected(first),
        'later_revisions': [{'commit': revision, 'committed_at': '2026-09-29T10:00:00Z',
                             'selected_files': selected({'pkg/revised.py': 'second\n'})}]})
    done, report = watch(tmp_path, env, '--records', str(core), '--records', str(service), '--licenses', str(licenses),
                         '--remote', str(tmp_path / 'remotes/{slug}'), '--diff-dir', str(tmp_path / 'diffs'))
    assert done.returncode == 1, done.stdout + done.stderr
    project, = report['projects']
    assert (project['head'], project['default_branch'], project['error']) == (head, 'main', None)
    files = {item['path']: item for item in project['files']}
    assert {path: item['status'] for path, item in files.items()} == {
        'pkg/edit.py': 'changed', 'pkg/gone.py': 'missing', 'pkg/moved.py': 'missing',
        'pkg/revised.py': 'unchanged', 'pkg/same.py': 'unchanged'}
    # pkg/revised.py differs from the pin; the later revision is its reference.
    revised = files['pkg/revised.py']['reference']
    assert (revised['commit'], revised['source'], revised['sha256']) == (revision, 'later_revisions', sha('second\n'))
    assert revised['records'] == [str(service.resolve())]
    assert revised['superseded'] == [{'commit': pin, 'sha256': sha('first\n'), 'source': 'selected_files'}]
    assert files['pkg/same.py']['reference']['records'] == [str(core.resolve()), str(service.resolve())]
    edit = files['pkg/edit.py']
    assert edit['compare_url'] == f'https://github.com/acme/widgets/compare/{pin}...{head}'
    assert edit['diff'] == {'added': 1, 'removed': 0, 'unified_lines': 6}
    assert edit['reference_verified'] is True
    assert '+three' in (tmp_path / 'diffs/acme__widgets/pkg/edit.py.diff').read_text()
    assert files['pkg/moved.py']['moved_to'] == ['lib/moved.py']
    assert files['pkg/gone.py']['moved_to'] == [] and files['pkg/gone.py']['reference_verified'] is True
    license, = project['licenses']
    assert (license['status'], license['head_file'], license['head_hint']) == ('unchanged', 'LICENSE', 'MIT')
    assert project['commits_behind'] == 2
    assert {ref['commit']: ref['commits_behind'] for ref in project['references']} == {pin: 2, revision: 1}
    assert report['summary']['files'] == {'total': 5, 'unchanged': 2, 'changed': 1, 'missing': 2}
    # Only the recorded files and the license were downloaded; the large blob stayed upstream.
    assert not cached(tmp_path, env, 'acme/widgets', up.git('rev-parse', f'{head}:data/large.bin'))
    assert cached(tmp_path, env, 'acme/widgets', up.git('rev-parse', f'{head}:pkg/same.py'))
    assert 'changed  pkg/edit.py  +1 -0' in done.stdout and 'moved to lib/moved.py' in done.stdout


def test_exit_zero_when_every_recorded_file_and_license_matches(tmp_path, env, licenses):
    up = Upstream(tmp_path / 'upstream', env)
    files = {'src/tool.py': 'VALUE = 1\n'}
    pin = up.commit({**files, 'LICENSE': MIT})
    head = up.commit({'README.md': 'an unrelated change\n'})
    (licenses / 'acme__tool__LICENSE.txt').write_text(MIT)
    record = write_record(tmp_path / 'UPSTREAM.json', {'repository': 'acme/tool', 'commit': pin,
                                                       'selected_files': selected(files)})
    done, report = watch(tmp_path, env, '--records', str(record), '--licenses', str(licenses),
                         '--remote-map', json.dumps({'acme/tool': (tmp_path / 'upstream').as_uri()}))
    assert done.returncode == 0, done.stdout + done.stderr
    project, = report['projects']
    assert (report['exit_code'], project['head'], project['commits_behind']) == (0, head, 1)
    assert [item['status'] for item in project['files']] == ['unchanged']
    assert report['alerts'] == [] and 'Exit 0' in done.stdout
    # A second run reuses the cache.
    again, report = watch(tmp_path, env, '--records', str(record), '--licenses', str(licenses),
                          '--remote-map', json.dumps({'acme/tool': (tmp_path / 'upstream').as_uri()}))
    assert again.returncode == 0 and report['projects'][0]['files'][0]['status'] == 'unchanged', again.stderr


def test_changed_or_removed_license_text_is_flagged(tmp_path, env, licenses):
    up = Upstream(tmp_path / 'remotes/acme__widgets', env)
    files = {'src/a.py': 'a = 1\n'}
    pin = up.commit({**files, 'LICENSE': MIT, 'NOTICE': 'Example notice\n'})
    up.commit({'LICENSE': BUSL, 'NOTICE': None})
    (licenses / 'acme__widgets__LICENSE.txt').write_text(MIT)
    (licenses / 'acme__widgets__NOTICE.txt').write_text('Example notice\n')
    record = write_record(tmp_path / 'UPSTREAM.json', {'repository': 'acme/widgets', 'commit': pin,
                                                       'selected_files': selected(files)})
    done, report = watch(tmp_path, env, '--records', str(record), '--licenses', str(licenses),
                         '--remote', str(tmp_path / 'remotes/{slug}'))
    assert done.returncode == 1, done.stdout + done.stderr
    project, = report['projects']
    assert [item['status'] for item in project['files']] == ['unchanged']
    license, notice = project['licenses']
    assert (license['status'], license['head_hint'], license['retained'][0]['hint']) == ('changed', 'BUSL', 'MIT')
    assert license['diff']['added'] and license['compare_url'].endswith(f'{pin}...{project["head"]}')
    assert (notice['kind'], notice['status'], notice['head_file']) == ('additional', 'missing', None)
    assert report['summary']['license_changes'] == 2
    assert done.stdout.count('!! LICENSE') == 2 and 'commercial use' in done.stdout


def test_blocked_tag_still_listed_is_reported_and_never_fetched(tmp_path, env, licenses):
    up = Upstream(tmp_path / 'memos', env)
    files = {'src/memos/templates/prompt.py': 'PROMPT = 1\n'}
    pin = up.commit(files)
    # Like the real v2.0.34: an annotated tag on a commit no branch contains.
    up.git('checkout', '-q', '-b', 'release')
    bad = up.commit({'src/memos/.sckit/linux-amd64/sckit': 'payload\n'})
    up.git('tag', '-a', '-m', 'release', 'v2.0.34')
    up.git('checkout', '-q', 'main')
    up.git('branch', '-q', '-D', 'release')
    up.commit({'README.md': 'docs\n'})
    record = write_record(tmp_path / 'UPSTREAM.json', {'repository': 'MemTensor/MemOS', 'commit': pin,
                                                       'selected_files': selected(files)})
    done, report = watch(tmp_path, env, '--records', str(record), '--licenses', str(licenses),
                         '--remote-map', json.dumps({'MemTensor/MemOS': str(tmp_path / 'memos')}))
    assert done.returncode == 1, done.stdout + done.stderr
    project, = report['projects']
    assert project['error'] is None and [item['status'] for item in project['files']] == ['unchanged']
    ref, = project['blocked_refs']
    assert (ref['ref'], ref['object'], ref['reported']) == ('refs/tags/v2.0.34', bad, 'SafeDep, 2026-09-23')
    assert report['summary']['blocked_refs_present'] == 1
    assert '!! BLOCKED REF PRESENT: MemTensor/MemOS refs/tags/v2.0.34' in done.stdout
    assert not cached(tmp_path, env, 'MemTensor/MemOS', bad)


def test_blocked_head_fails_loudly_before_anything_is_fetched(tmp_path, env, licenses):
    up = Upstream(tmp_path / 'remotes/acme__widgets', env)
    files = {'src/a.py': 'a = 1\n'}
    pin = up.commit(files)
    head = up.commit({'src/a.py': 'a = 2\n'})
    record = write_record(tmp_path / 'UPSTREAM.json', {'repository': 'acme/widgets', 'commit': pin,
                                                       'selected_files': selected(files)})
    done, report = watch(tmp_path, env, '--records', str(record), '--licenses', str(licenses),
                         '--remote', str(tmp_path / 'remotes/{slug}'), '--block', f'acme/widgets@{head[:10]}')
    assert done.returncode == 2, done.stdout + done.stderr
    project, = report['projects']
    assert (project['status'], project['error']['kind'], project['files']) == ('blocked', 'blocked', [])
    assert head in project['error']['message']
    assert not list((tmp_path / 'cache').glob('acme__widgets-*.git'))
    assert '!! BLOCKED: acme/widgets: HEAD' in done.stdout


def test_head_carrying_the_compromise_files_is_not_read(tmp_path, env, licenses):
    up = Upstream(tmp_path / 'memos', env)
    files = {'src/memos/embedders/cache.py': 'CACHE = 1\n'}
    pin = up.commit(files)
    head = up.commit({'src/memos/.sckit/linux-amd64/sckit': 'payload\n'})
    record = write_record(tmp_path / 'UPSTREAM.json', {'repository': 'MemTensor/MemOS', 'commit': pin,
                                                       'selected_files': selected(files)})
    done, report = watch(tmp_path, env, '--records', str(record), '--licenses', str(licenses),
                         '--remote-map', json.dumps({'MemTensor/MemOS': str(tmp_path / 'memos')}))
    assert done.returncode == 2, done.stdout + done.stderr
    project, = report['projects']
    assert project['error']['kind'] == 'blocked' and '.sckit' in project['error']['message']
    assert project['files'] == []
    for path in ('src/memos/.sckit/linux-amd64/sckit', 'src/memos/embedders/cache.py'):
        assert not cached(tmp_path, env, 'MemTensor/MemOS', up.git('rev-parse', f'{head}:{path}'))


def test_missing_repository_is_an_error_and_the_others_are_still_checked(tmp_path, env, licenses):
    up = Upstream(tmp_path / 'remotes/acme__widgets', env)
    files = {'src/a.py': 'a = 1\n'}
    pin = up.commit(files)
    record = write_record(tmp_path / 'UPSTREAM.json',
                          {'repository': 'acme/widgets', 'commit': pin, 'selected_files': selected(files)},
                          {'repository': 'acme/vanished', 'commit': pin, 'selected_files': selected(files)})
    done, report = watch(tmp_path, env, '--records', str(record), '--licenses', str(licenses),
                         '--remote', str(tmp_path / 'remotes/{slug}'))
    assert done.returncode == 2, done.stdout + done.stderr
    projects = {project['repository']: project for project in report['projects']}
    assert projects['acme/vanished']['error']['kind'] == 'missing_repository'
    assert [item['status'] for item in projects['acme/widgets']['files']] == ['unchanged']
    assert report['summary']['errors'] == 1 and 'Exit 2' in done.stdout
    done, report = watch(tmp_path, env, '--records', str(record), '--licenses', str(licenses),
                         '--remote', str(tmp_path / 'remotes/{slug}'), '--only', 'acme/widgets')
    assert done.returncode == 0 and [p['repository'] for p in report['projects']] == ['acme/widgets'], done.stderr


def test_invalid_records_are_an_error(tmp_path, env, licenses):
    record = write_record(tmp_path / 'UPSTREAM.json', {'repository': 'acme/widgets', 'commit': 'main',
                                                       'selected_files': []})
    done, report = watch(tmp_path, env, '--records', str(record), '--licenses', str(licenses))
    assert done.returncode == 2 and report is None
    assert 'commit must be a full hexadecimal id' in done.stderr


def test_the_newest_later_revision_is_the_reference(tmp_path):
    pin, older, newer = 'a' * 40, 'b' * 40, 'c' * 40

    def entry(digit):
        return [{'path': 'x.py', 'sha256': digit * 64, 'inspection': 'test'}]

    # 17:05:35Z is newer than 18:30+02:00 (16:30Z), although it sorts first as text.
    record = write_record(tmp_path / 'UPSTREAM.json', {
        'repository': 'acme/widgets', 'commit': pin, 'selected_files': entry('0'),
        'later_revisions': [{'commit': newer, 'committed_at': '2026-09-29T17:05:35Z', 'selected_files': entry('2')},
                            {'commit': older, 'committed_at': '2026-09-29T18:30:00+02:00', 'selected_files': entry('1')}]})
    project, = upstream_watch.load_records([record])
    reference = project['references']['x.py']
    assert (reference['commit'], reference['sha256'], reference['source']) == (newer, '2' * 64, 'later_revisions')
    assert [item['commit'] for item in reference['superseded']] == [pin, older]


def test_the_shipped_records_load():
    projects = upstream_watch.load_records([ROOT / 'UPSTREAM.json', ROOT.parent / 'graphicle-memory/UPSTREAM.json'])
    assert projects and all(project['references'] and not project['warnings'] for project in projects)


def test_license_hints_name_terms_that_matter_for_commercial_use():
    assert upstream_watch.license_hint(MIT.encode()) == 'MIT'
    assert upstream_watch.license_hint(BUSL.encode()) == 'BUSL'
    split = b'Content under any directory named `ee/` has its own license.\nApache License 2.0\n'
    assert upstream_watch.license_hint(split) == 'Apache-2.0 + ee/ or enterprise terms'
