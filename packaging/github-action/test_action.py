#!/usr/bin/env python3
"""Tests of the review action's scripts, with stand-ins for curl, graphicle
and gh on PATH, so they run offline with bash and jq:
python3 packaging/github-action/test_action.py

They check the event gate (forks and runs without a key skip,
pull_request_target and other events fail), the pinned install (a wrong
SHA-256 installs nothing), the review step's configuration and command, and
the posting (one COMMENT review, the body fallback, nothing without
comments when asked)."""
import hashlib
import io
import json
import os
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
VERSION = '0.1.9'
TARGET = {'Linux-x86_64': 'x86_64-unknown-linux-gnu', 'Linux-aarch64': 'aarch64-unknown-linux-gnu',
          'Darwin-arm64': 'aarch64-apple-darwin', 'Darwin-x86_64': 'x86_64-apple-darwin'}[
    f'{os.uname().sysname}-{os.uname().machine}']


class Runner:
    """A temporary runner: RUNNER_TEMP, the output files and a bin folder of stand-ins."""

    def __init__(self, root):
        self.root = Path(root)
        self.temp = self.root / 'runner-temp'
        self.bin = self.root / 'bin'
        self.temp.mkdir()
        self.bin.mkdir()
        self.output = self.root / 'output'
        self.path_file = self.root / 'path'
        self.summary = self.root / 'summary'
        self.calls = self.root / 'calls'
        for f in (self.output, self.path_file, self.summary, self.calls):
            f.write_text('')

    def tool(self, name, body):
        script = self.bin / name
        script.write_text('#!/usr/bin/env bash\n' + body)
        script.chmod(0o755)

    def run(self, script, **env):
        full = {'PATH': f'{self.bin}:/usr/bin:/bin:/usr/local/bin', 'HOME': str(self.root),
                'RUNNER_TEMP': str(self.temp), 'GITHUB_OUTPUT': str(self.output),
                'GITHUB_PATH': str(self.path_file), 'GITHUB_STEP_SUMMARY': str(self.summary),
                'CALLS': str(self.calls), **env}
        return subprocess.run(['bash', str(HERE / script)], env=full, cwd=self.root,
                              capture_output=True, text=True)

    def outputs(self):
        return dict(line.split('=', 1) for line in self.output.read_text().splitlines() if '=' in line)


class Gate(unittest.TestCase):
    def gate(self, **env):
        with tempfile.TemporaryDirectory() as root:
            runner = Runner(root)
            done = runner.run('gate.sh', **{'EVENT': 'pull_request', 'HEAD_REPO': 'org/repo',
                                            'BASE_REPO': 'org/repo', 'HAS_KEY': 'true', **env})
            return done, runner.outputs()

    def test_a_pull_request_with_a_key_runs(self):
        done, outputs = self.gate()
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(outputs, {'run': 'true'})

    def test_a_fork_and_a_missing_key_skip(self):
        for env, words in (({'HEAD_REPO': 'someone/repo'}, 'fork'), ({'HEAD_REPO': ''}, 'fork'),
                           ({'HAS_KEY': 'false'}, 'no model key')):
            done, outputs = self.gate(**env)
            self.assertEqual(done.returncode, 0, done.stderr)
            self.assertEqual(outputs, {'run': 'false'})
            self.assertIn(words, done.stdout)

    def test_pull_request_target_and_other_events_fail(self):
        for event in ('pull_request_target', 'push', 'issue_comment'):
            done, outputs = self.gate(EVENT=event)
            self.assertEqual(done.returncode, 1)
            self.assertEqual(outputs, {})
            self.assertIn('::error::', done.stdout)


def archive_bytes():
    """A release archive whose graphicle prints its version."""
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode='w:gz') as tar:
        data = f'#!/usr/bin/env bash\necho "graphicle {VERSION}"\n'.encode()
        info = tarfile.TarInfo(f'graphicle-{VERSION}-{TARGET}/graphicle')
        info.size, info.mode = len(data), 0o755
        tar.addfile(info, io.BytesIO(data))
    return buffer.getvalue()


class Install(unittest.TestCase):
    def install(self, sha256=None, version=VERSION):
        root = tempfile.TemporaryDirectory()
        self.addCleanup(root.cleanup)
        runner = Runner(root.name)
        archive = self.root_archive = Path(root.name) / 'release.tar.gz'
        archive.write_bytes(archive_bytes())
        # curl copies the release archive to --output and records the URL.
        runner.tool('curl', f'''out=""; url=""
while [ $# -gt 0 ]; do case "$1" in --output) out=$2; shift 2;; --proto|--retry) shift 2;; -*) shift;; *) url=$1; shift;; esac; done
echo "$url" >> "$CALLS"; cp "{archive}" "$out"
''')
        sha = sha256 or hashlib.sha256(archive.read_bytes()).hexdigest()
        done = runner.run('install.sh', VERSION=version, SHA256=sha)
        return runner, done

    def test_the_pinned_archive_is_checked_and_put_on_path(self):
        runner, done = self.install()
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(runner.calls.read_text().strip(),
                         f'https://github.com/graphicle-ai/graphicle/releases/download/v{VERSION}/'
                         f'graphicle-{VERSION}-{TARGET}.tar.gz')
        folder = runner.path_file.read_text().strip()
        self.assertTrue((Path(folder) / 'graphicle').exists())
        self.assertIn(f'graphicle {VERSION}', done.stdout)

    def test_a_wrong_sha256_installs_nothing(self):
        runner, done = self.install(sha256='0' * 64)
        self.assertEqual(done.returncode, 1)
        self.assertIn('not the pinned', done.stdout)
        self.assertEqual(runner.path_file.read_text(), '')
        self.assertEqual(list((runner.temp / 'graphicle-bin').iterdir()), [])

    def test_a_version_or_sha256_that_is_not_one_is_refused(self):
        for version, sha in (('latest', None), ('0.1.9; rm -rf /', None), (VERSION, 'abc')):
            runner, done = self.install(sha256=sha, version=version)
            self.assertEqual(done.returncode, 1)
            self.assertEqual(runner.calls.read_text(), '')


PAYLOAD = {'commit_id': 'f' * 40, 'event': 'COMMENT', 'body': 'Graphicle review.',
           'comments': [{'path': 'src/a.go', 'line': 5, 'side': 'RIGHT', 'body': '**major**: x\n\nEvidence: `src/a.go:5`'}]}


class Review(unittest.TestCase):
    def review(self, **env):
        root = tempfile.TemporaryDirectory()
        self.addCleanup(root.cleanup)
        runner = Runner(root.name)
        # graphicle records its arguments and the key it sees; review prints the payload.
        runner.tool('graphicle', f'''echo "key=${{MODEL_API_KEY:-none}} $*" >> "$CALLS"
for a in "$@"; do if [ "$a" = review ]; then echo '{json.dumps(PAYLOAD)}'; fi; done
''')
        full = {'MODEL_API_KEY': 'k', 'MODEL': 'muse-spark-1.3', 'PRICE': '{"input":1.25,"output":4.25,"cached_input":0.15}',
                'MAX_COST': '0.50', 'BUDGET': '200000', 'MAX_COMMENTS': '10', 'BASE_SHA': 'a' * 40, 'HEAD_SHA': 'b' * 40, **env}
        return runner, runner.run('review.sh', **full)

    def test_the_profile_is_capped_and_the_review_written(self):
        runner, done = self.review()
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        calls = runner.calls.read_text().splitlines()
        data, config = runner.temp / 'graphicle-data', runner.temp / 'graphicle-models.json'
        prefix = f'key=k --data-dir {data} --config {config} '
        self.assertEqual([c[len(prefix):] for c in calls], [
            'config set model meta:muse-spark-1.3',
            'config set models.meta.price {"input":1.25,"output":4.25,"cached_input":0.15}',
            'config set models.meta.output_limit 16000',
            'config set models.meta.max_cost_per_call 0.50',
            'sync . --quiet',
            f'review --base {"a" * 40} --head {"b" * 40} --format github --budget 200000 --max-comments 10',
            'usage'])
        self.assertEqual(json.loads((runner.temp / 'graphicle-review.json').read_text()), PAYLOAD)
        self.assertEqual(runner.outputs(), {'comments': '1'})

    def test_bad_inputs_are_refused_before_anything_runs(self):
        for env in ({'MODEL': 'muse-spark-1.3-contributor'}, {'MAX_COST': '1e9'},
                    {'MAX_COST': '$(id)'}, {'BUDGET': '200k'}, {'MAX_COMMENTS': '-1'}):
            runner, done = self.review(**env)
            self.assertEqual(done.returncode, 1, env)
            self.assertIn('::error::', done.stdout)
            self.assertEqual(runner.calls.read_text(), '')


class Post(unittest.TestCase):
    def post(self, payload, fail_first=False, post_empty='true'):
        root = tempfile.TemporaryDirectory()
        self.addCleanup(root.cleanup)
        runner = Runner(root.name)
        (runner.temp / 'graphicle-review.json').write_text(json.dumps(payload))
        # gh records each call and the payload it would send; it can refuse the first.
        runner.tool('gh', f'''n=$(grep -c . "$CALLS" || true)
input=""; prev=""; for a in "$@"; do [ "$prev" = --input ] && input=$a; prev=$a; done
echo "token=$GH_TOKEN $* $(jq -c . "$input")" >> "$CALLS"
if [ "{'yes' if fail_first else 'no'}" = yes ] && [ "$n" = 0 ]; then exit 1; fi
''')
        done = runner.run('post.sh', GH_TOKEN='t', REPOSITORY='org/repo', PULL='7', POST_EMPTY=post_empty)
        calls = runner.calls.read_text().splitlines()
        sent = [json.loads(c[c.index('{'):]) for c in calls]
        return runner, done, calls, sent

    def test_one_comment_review_is_posted(self):
        runner, done, calls, sent = self.post(dict(PAYLOAD, event='APPROVE'))
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(len(calls), 1)
        self.assertTrue(calls[0].startswith('token=t api --method POST repos/org/repo/pulls/7/reviews --input '))
        self.assertEqual(sent[0]['event'], 'COMMENT')
        self.assertEqual(sent[0]['comments'], PAYLOAD['comments'])
        self.assertIn('Graphicle review.', runner.summary.read_text())

    def test_refused_inline_comments_go_into_the_body(self):
        runner, done, calls, sent = self.post(PAYLOAD, fail_first=True)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(len(calls), 2)
        self.assertEqual(sent[1]['comments'], [])
        self.assertEqual(sent[1]['event'], 'COMMENT')
        self.assertIn('- `src/a.go:5`: **major**: x Evidence: `src/a.go:5`', sent[1]['body'])

    def test_no_comments_posts_nothing_when_asked(self):
        runner, done, calls, _ = self.post(dict(PAYLOAD, comments=[]), post_empty='false')
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(calls, [])
        runner, done, calls, sent = self.post(dict(PAYLOAD, comments=[]))
        self.assertEqual(len(calls), 1)


if __name__ == '__main__':
    unittest.main()
