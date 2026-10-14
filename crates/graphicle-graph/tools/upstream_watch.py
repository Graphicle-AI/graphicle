#!/usr/bin/env python3
"""Report what upstream Graphify changed since the pinned port (nightly job).

The pin comes from the adaptation headers in src/ (`// Adapted from Graphify
<version> (<sha>)`). The GitHub compare API gives the commits and changed files
from the pin to the upstream branch head, and the releases API the tags
published after the pin. Every changed upstream file is mapped to the Rust
modules whose `// Upstream:` header names it, so a reviewer sees which ports
need a look. Nothing in the tree is edited.

Outputs in --out: upstream-watch.json (full report) and upstream-watch.md
(summary; also appended to --summary, e.g. "$GITHUB_STEP_SUMMARY").

Usage:
  upstream_watch.py [--repo Graphify-Labs/graphify] [--ref v8] [--out DIR]
                    [--compare-json FILE] [--summary FILE] [--fail-on-change]
--compare-json reuses a saved compare response (offline; no release lookup).
Auth: GITHUB_TOKEN or GH_TOKEN when set (higher rate limit), else anonymous.
The compare API lists at most 250 commits and 300 files; the report says when
a list was cut.
"""
import argparse
import collections
import json
import os
import re
import sys
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
API = 'https://api.github.com'
PIN = re.compile(r'Adapted from Graphify (\S+) \(([0-9a-f]{7,40})\)')
UPSTREAM = re.compile(r'^//\s*Upstream:\s*(.+)$')


def pin():
    """(version, sha) named by most adaptation headers."""
    seen = collections.Counter()
    for path in sorted((ROOT / 'src').rglob('*.rs')):
        head = path.read_text(errors='replace')[:400]
        seen.update(PIN.findall(head))
    if not seen:
        sys.exit('no "Adapted from Graphify <version> (<sha>)" header under src/')
    return seen.most_common(1)[0][0]


def upstream_map():
    """Upstream file -> Rust files whose header names it."""
    out = collections.defaultdict(list)
    for path in sorted((ROOT / 'src').rglob('*.rs')):
        for line in path.read_text(errors='replace').splitlines()[:6]:
            m = UPSTREAM.match(line.strip())
            if m:
                for name in m.group(1).split(','):
                    out[name.strip()].append(path.relative_to(ROOT).as_posix())
    return out


def get(url):
    request = urllib.request.Request(url, headers={'Accept': 'application/vnd.github+json',
                                                  'User-Agent': 'graphicle-upstream-watch'})
    token = os.environ.get('GITHUB_TOKEN') or os.environ.get('GH_TOKEN')
    if token:
        request.add_header('Authorization', f'Bearer {token}')
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.load(response)


def report(compare, releases, version, sha, repo, ref):
    mapping = upstream_map()
    files = []
    for f in compare.get('files') or []:
        name = f.get('filename', '')
        files.append({'filename': name, 'status': f.get('status'), 'additions': f.get('additions', 0),
                      'deletions': f.get('deletions', 0), 'rust_modules': mapping.get(name, [])})
    commits = [{'sha': c['sha'][:12], 'date': c['commit']['committer']['date'],
                'title': c['commit']['message'].splitlines()[0]} for c in compare.get('commits') or []]
    base_date = ((compare.get('base_commit') or {}).get('commit') or {}).get('committer', {}).get('date', '')
    after = [{'tag': r.get('tag_name'), 'name': r.get('name'), 'published_at': r.get('published_at')}
             for r in releases if base_date and (r.get('published_at') or '') > base_date
             and (r.get('tag_name') or '').lstrip('v') != version.lstrip('v')]
    return {
        'repo': repo, 'ref': ref, 'pinned_version': version, 'pinned_sha': sha,
        'pinned_commit_date': base_date,
        'head_sha': (compare.get('commits') or [{}])[-1].get('sha', sha)[:12] if compare.get('commits') else sha,
        'ahead_by': compare.get('ahead_by', 0), 'behind_by': compare.get('behind_by', 0),
        'commits_listed': len(commits), 'commits_truncated': compare.get('ahead_by', 0) > len(commits),
        'files_listed': len(files), 'files_truncated': len(files) >= 300,
        'releases_after_pin': after,
        'rust_modules_to_review': sorted({m for f in files for m in f['rust_modules']}),
        'unmapped_files': sorted(f['filename'] for f in files if not f['rust_modules']),
        'commits': commits, 'files': files,
    }


def markdown(r):
    lines = [f"## Upstream {r['repo']}@{r['ref']} vs pin {r['pinned_version']} ({r['pinned_sha']})", '']
    if not r['ahead_by']:
        lines.append('No upstream commits since the pin.')
        return '\n'.join(lines) + '\n'
    lines += [f"- {r['ahead_by']} commit(s) ahead (head {r['head_sha']}); {r['files_listed']} file(s) changed"
              + (' (list cut by the API)' if r['files_truncated'] or r['commits_truncated'] else ''),
              f"- Releases after the pin: {', '.join(x['tag'] for x in r['releases_after_pin']) or 'none'}",
              f"- Rust modules to review: {len(r['rust_modules_to_review'])}", '',
              '| upstream file | +/- | Rust modules |', '|---|---|---|']
    for f in sorted(r['files'], key=lambda f: -(f['additions'] + f['deletions']))[:40]:
        mods = ', '.join(f['rust_modules']) or '(none)'
        lines.append(f"| {f['filename']} | +{f['additions']}/-{f['deletions']} | {mods} |")
    lines += ['', '| commit | date | title |', '|---|---|---|']
    for c in r['commits'][-30:]:
        lines.append(f"| {c['sha']} | {c['date'][:10]} | {c['title'].replace('|', '/')} |")
    return '\n'.join(lines) + '\n'


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--repo', default='Graphify-Labs/graphify')
    ap.add_argument('--ref', default='v8')
    ap.add_argument('--out', default='upstream-watch')
    ap.add_argument('--compare-json', type=Path)
    ap.add_argument('--summary', type=Path)
    ap.add_argument('--fail-on-change', action='store_true')
    args = ap.parse_args()
    version, sha = pin()
    if args.compare_json:
        compare, releases = json.loads(args.compare_json.read_text()), []
    else:
        compare = get(f'{API}/repos/{args.repo}/compare/{sha}...{args.ref}')
        releases = get(f'{API}/repos/{args.repo}/releases?per_page=30')
    r = report(compare, releases, version, sha, args.repo, args.ref)
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    (out / 'upstream-watch.json').write_text(json.dumps(r, indent=2) + '\n')
    md = markdown(r)
    (out / 'upstream-watch.md').write_text(md)
    if args.summary:
        with args.summary.open('a') as stream:
            stream.write(md)
    print(md, end='')
    return 1 if args.fail_on_change and r['ahead_by'] else 0


if __name__ == '__main__':
    sys.exit(main())
