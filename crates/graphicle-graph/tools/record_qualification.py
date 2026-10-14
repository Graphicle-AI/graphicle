#!/usr/bin/env python3
"""Record every application module only after one complete qualification passes."""
import argparse
import datetime
import json
import subprocess
import sys
from pathlib import Path
from qualification import ROOT, sha, validate, harness_sources

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('report', type=Path)
args = parser.parse_args()
report = json.loads(args.report.read_text())
assert report['passed'] and all(row['exit_code'] == 0 for row in report['checks'])
expected_checks = {p.name for p in (ROOT / 'tools').glob('check_*.py')
                   if p.name not in {'check_llm_execution.py', 'check_prs_execution.py'}}
assert {row['check'] for row in report['checks']} == expected_checks, 'Full suite required'
manifest = validate(ROOT / report['manifest'])
assert report['binary_sha256'] == manifest['binary_sha256']
assert report['harness_files'] == harness_sources(), 'Qualification tools changed'
ledger_path = ROOT / 'migration/FILES.json'
ledger = json.loads(ledger_path.read_text())
receipts = {}
for path in sorted((ROOT / 'migration/receipts').glob('*.json')):
    data = json.loads(path.read_text())
    if not isinstance(data, dict) or data.get('passed') is not True:
        continue
    if data.get('qualification_manifest') != report['manifest']:
        continue
    if data.get('binary_sha256') != manifest['binary_sha256'] or data.get('source_set_sha256') != manifest['source_set_sha256']:
        continue
    assert data['harness_files'] == report['harness_files'], path
    assert data['qualification_manifest_sha256'] == sha(ROOT / report['manifest']), path
    for name, expected in data['rust_files'].items():
        assert sha(ROOT / name) == expected, (path, name)
    receipts.setdefault(data['source'], []).append((path, data))
missing = []
for row in ledger['files']:
    if row['category'] != 'application_python':
        continue
    evidence = receipts.get(row['path'], [])
    if not evidence:
        missing.append(row['path'])
        continue
    for path, data in evidence:
        assert data['source_sha256'] == row['sha256'] and data['upstream_commit'] == ledger['upstream_commit'], path
    files = list(dict.fromkeys(name for _, data in evidence for name in data['rust_files']))
    row.update(status='ported_verified', rust_target=files[0], rust_files=files,
               evidence=[str(path.relative_to(ROOT)) for path, _ in evidence],
               checked_utc=datetime.datetime.now(datetime.timezone.utc).isoformat())
assert not missing, f'No current passing receipt for {missing}'
ledger_path.write_text(json.dumps(ledger, indent=2, ensure_ascii=False) + '\n')
raise SystemExit(subprocess.call([sys.executable, str(ROOT / 'tools/report_migration.py')]))
