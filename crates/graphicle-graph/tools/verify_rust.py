#!/usr/bin/env python3
"""Repeatable formatting, strict Clippy and native test gate for the three crates.

The engine, the memory crate (../graphicle-memory) and the program (../graphicle).
"""
import concurrent.futures
import datetime
import hashlib
import json
import re
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CRATES = [ROOT, ROOT.parent / 'graphicle-memory', ROOT.parent / 'graphicle']


def sources():
    paths = set()
    for crate in CRATES:
        paths.update(crate / name for name in ['Cargo.toml', 'Cargo.lock', 'build.rs', '.cargo/config.toml', '.cargo/config'] if (crate / name).is_file())
        for folder in ['src', 'tests', 'examples', 'vendor', 'cli']:
            paths.update(p for p in (crate / folder).rglob('*') if p.is_file() and not {'target', '__pycache__'}.intersection(p.parts))
    return {str(p.relative_to(ROOT.parent)): hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(paths)}


def main():
    before = sources()
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%SZ')
    directory = ROOT / 'migration/quality' / stamp
    directory.mkdir(parents=True)

    def check(crate):
        # The engine with its opt-in features too, so `bedrock` and
        # `transcription` stay built, linted and tested.
        features = ['--features', 'postgres,bedrock,transcription'] if crate == ROOT else []
        commands = [
            # Format all first-party workspace packages without imposing our
            # style on vendored dependencies.
            ['cargo', 'fmt', *(['--package', 'graphicle', '--package', 'graphicle-graph']
                               if crate == ROOT else []), '--check'],
            ['cargo', 'clippy', '--offline', '--locked', '--all-targets', *features, '--', '-D', 'warnings'],
            ['cargo', 'test', '--offline', '--locked', '--all-targets', *features],
        ]
        rows = []
        for command in commands:
            log = directory / f'{crate.name}-{command[1]}.log'
            with log.open('w') as stream:
                result = subprocess.run(command, cwd=crate, stdout=stream, stderr=subprocess.STDOUT)
            row = {'crate': crate.name, 'command': command, 'exit_code': result.returncode,
                   'log': str(log.relative_to(ROOT))}
            if command[1] == 'test':
                row['tests_passed'] = sum(map(int, re.findall(r'test result: ok\. (\d+) passed;', log.read_text())))
            rows.append(row)
            print(json.dumps(row), flush=True)
            if result.returncode:
                break
        return rows

    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
        results = [row for rows in executor.map(check, CRATES) for row in rows]
    unchanged = sources() == before
    report = {'source_files': before, 'sources_unchanged': unchanged, 'checks': results,
              'passed': unchanged and len(results) == 3 * len(CRATES) and all(r['exit_code'] == 0 for r in results),
              'rustc': subprocess.check_output(['rustc', '--version'], text=True).strip()}
    target = directory / 'results.json'
    target.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({'report': str(target), 'passed': report['passed']}), flush=True)
    return 0 if report['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
