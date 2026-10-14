#!/usr/bin/env python3
"""Run the original module oracles against one source-bound native build.

Independent test processes may run concurrently. The final report contains
every command, exit status, duration, log path and the immutable build manifest.
"""
import argparse, concurrent.futures, datetime, json, os, subprocess, sys, tempfile, time
from pathlib import Path
import naming, qualification

ROOT = qualification.ROOT

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--manifest', type=Path)
    parser.add_argument('--profile', choices=['debug', 'release'], default='release')
    parser.add_argument('--jobs', type=int, default=2)
    parser.add_argument('--checks', nargs='*')
    args = parser.parse_args()
    if args.manifest:
        manifest = args.manifest.resolve(); record = qualification.validate(manifest)
        binary = Path(record['binary_path'])
    else:
        binary, manifest = qualification.build(args.profile)
        record = qualification.validate(manifest, binary)
    # Upstream tests compare subprocess stdout (including Node's console.log
    # of numbers) exactly. A desktop/CI FORCE_COLOR setting must not inject
    # terminal escape codes into that machine-readable fixture output.
    # Inherited reference (GRAPHIFY_*) and engine (GRAPHICLE_*) settings are
    # dropped, so a developer's shell cannot perturb only one side; checks set
    # what they compare. The harness's own variables are kept (tools/naming.py).
    environment = {**naming.scrubbed(os.environ), 'FORCE_COLOR': '0', 'PY_COLORS': '0',
                   'GRAPHICLE_GRAPH_BINARY': str(binary), 'GRAPHICLE_QUALIFICATION_MANIFEST': str(manifest)}
    helpers = {'check_llm_execution.py', 'check_prs_execution.py'}
    checks = [ROOT / 'tools' / name for name in args.checks] if args.checks else sorted(
        p for p in (ROOT / 'tools').glob('check_*.py') if p.name not in helpers)
    harness_files = qualification.harness_sources()
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%SZ')
    directory = ROOT / 'migration/qualification' / f'run-{stamp}'
    directory.mkdir(parents=True)
    # Fixtures that deliberately have no VCS root must not inherit this
    # checkout's Git root, ignore files, package manifests or project markers.
    # Keep only logs in the qualification directory, and isolate each oracle
    # below a fresh system-temporary parent outside every VCS checkout.
    temporary = tempfile.TemporaryDirectory(prefix='graphify-oracles-')
    temporary_root = Path(temporary.name).resolve()
    (ROOT / 'migration/receipts').mkdir(parents=True, exist_ok=True)
    def run(check):
        start = time.monotonic(); log = directory / (check.stem + '.log')
        command = [sys.executable, str(check)]
        # Some pinned tests leave NamedTemporaryFile(delete=False) project
        # markers behind. Sharing /tmp can make a later XAML fixture discover
        # that marker in an ancestor and scan unrelated repositories. Keep
        # each oracle's temporary inputs together and clean them even on failure.
        with tempfile.TemporaryDirectory(prefix=check.stem+'-', dir=temporary_root) as scratch, log.open('w') as stream:
            check_environment = {**environment, 'TMPDIR': scratch, 'TMP': scratch, 'TEMP': scratch}
            result = subprocess.run(command, cwd=ROOT, env=check_environment, stdout=stream, stderr=subprocess.STDOUT)
        row = {'check': check.name, 'command': command, 'exit_code': result.returncode,
               'seconds': time.monotonic()-start, 'log': str(log.relative_to(ROOT)),
               'check_sha256': qualification.sha(check)}
        print(json.dumps(row), flush=True)
        return row
    try:
        if any((parent / marker).exists()
               for parent in (temporary_root, *temporary_root.parents)
               for marker in ('.git', '.hg', '.svn')):
            raise RuntimeError('Oracle TMPDIR must be outside a VCS checkout')
        with concurrent.futures.ThreadPoolExecutor(max_workers=max(1,args.jobs)) as executor:
            results = list(executor.map(run, checks))
    finally:
        temporary.cleanup()
    qualification.validate(manifest, binary)
    assert qualification.harness_sources() == harness_files, 'Qualification tools changed during the run'
    summary = {'manifest': str(manifest.relative_to(ROOT)), 'binary_sha256': record['binary_sha256'],
               'source_set_sha256': record['source_set_sha256'], 'checks': results,
               'harness_files': harness_files,
               'passed': all(row['exit_code']==0 for row in results)}
    (directory/'results.json').write_text(json.dumps(summary,indent=2)+'\n')
    print(json.dumps({'passed':summary['passed'],'report':str(directory/'results.json')}),flush=True)
    return 0 if summary['passed'] else 1

if __name__ == '__main__': raise SystemExit(main())
