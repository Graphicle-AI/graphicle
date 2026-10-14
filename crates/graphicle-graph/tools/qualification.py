"""Bind a Cargo build and every comparison receipt to one immutable source set."""
import datetime, hashlib, json, os, subprocess, tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

def sources():
    files = [ROOT / name for name in ('Cargo.toml', 'Cargo.lock', 'build.rs') if (ROOT / name).exists()]
    files += [path for path in (ROOT / 'src').rglob('*') if path.is_file()]
    files += list((ROOT / 'tests').glob('*.rs'))
    # The executable's package: its manifest pins the parser runtime.
    files += [ROOT / 'cli/Cargo.toml', *(ROOT / 'cli/tests').glob('*.rs')]
    # Standalone profiling examples are not linked into this release binary.
    # Their formatting, compilation and source hashes belong to verify_rust.
    for dependency in [ROOT / 'vendor']:
        if dependency.exists():
            files += [path for path in dependency.rglob('*') if path.is_file() and 'target' not in path.parts]
    return {os.path.relpath(path, ROOT): sha(path) for path in sorted(set(files))}

def harness_sources():
    files = list((ROOT / 'tools').glob('*.py'))
    files += [ROOT / 'migration/upstream-review' / name for name in ('adaptations.json', 'since-pinned.json')]
    return {os.path.relpath(path, ROOT): sha(path) for path in sorted(files) if path.is_file()}

def validate(path, binary=None):
    path = Path(path).resolve()
    record = json.loads(path.read_text())
    changed = [name for name, digest in record['source_files'].items() if not (ROOT / name).is_file() or sha(ROOT / name) != digest]
    assert not changed, f'Qualified sources changed: {changed}'
    assert record['source_files'] == sources(), 'Qualified source inventory changed'
    if binary is not None:
        assert sha(Path(binary)) == record['binary_sha256'], 'Qualified binary changed'
    return record

# The qualified build: the default features plus `postgres` and the two
# opt-in features the parity checks exercise, `bedrock` (check_llm_execution)
# and `transcription` (check_transcribe).
FEATURES = ['postgres', 'bedrock', 'transcription']

def build(profile='release'):
    before = sources()
    command = ['cargo', 'build', '--offline', '--locked', '--features', ','.join(FEATURES)]
    if profile == 'release': command.append('--release')
    subprocess.run(command, cwd=ROOT, check=True)
    assert sources() == before, 'Sources changed during the Cargo build; rebuild before qualification'
    metadata = json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--offline', '--locked', '--no-deps', '--format-version=1'],
        cwd=ROOT, text=True))
    source = Path(metadata['target_directory']) / ('release' if profile == 'release' else 'debug') / 'graphicle-graph'
    digest = sha(source)
    binary = Path(tempfile.gettempdir()) / 'graphicle-graph-builds' / digest / 'graphicle-graph'
    binary.parent.mkdir(parents=True, exist_ok=True)
    if not binary.exists(): binary.write_bytes(source.read_bytes()); binary.chmod(0o755)
    assert sha(binary) == digest
    source_digest = hashlib.sha256(json.dumps(before, sort_keys=True).encode()).hexdigest()
    record = {'binary_path': str(binary), 'binary_sha256': digest, 'profile': profile,
              'source_files': before, 'source_set_sha256': source_digest,
              'features': ['default', *FEATURES], 'build_command': command,
              'rustc': subprocess.check_output(['rustc', '--version'], text=True).strip(),
              'built_utc': datetime.datetime.now(datetime.timezone.utc).isoformat()}
    path = ROOT / 'migration/qualification' / f'{digest}-{source_digest[:12]}.json'
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(record, indent=2) + '\n')
    return binary, path

if __name__ == '__main__':
    import argparse
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--profile', choices=['debug', 'release'], default='release')
    args = parser.parse_args()
    binary, manifest = build(args.profile)
    print(json.dumps({'binary': str(binary), 'manifest': str(manifest)}))
