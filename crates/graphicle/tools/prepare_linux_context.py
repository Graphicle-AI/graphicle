#!/usr/bin/env python3
"""Prepare offline Docker inputs from the exact locked crates and cached grammar sources.

--edition core vendors this crate's locked dependencies into .vendor (for
deploy/Dockerfile); --edition pro vendors those of the commercial edition's
crate given with --crate into its own .vendor (for its Dockerfile). Both editions use the
parser sources copied to deploy/.offline. No registry credentials are copied.
Large generated inputs remain gitignored. The parser sources come from the
folder tools/prepare_parser_sources.py prepares (else from the cache a build
without that folder unpacked).
"""
from pathlib import Path
import argparse,hashlib,json,shutil,subprocess,tomllib
ROOT=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser(description=__doc__.splitlines()[0]);parser.add_argument('--edition',choices=['core','pro'],required=True);parser.add_argument('--crate',type=Path,help='The commercial edition\'s crate (--edition pro)')
arguments=parser.parse_args();edition=arguments.edition
if (edition=='pro')!=(arguments.crate is not None):parser.error('--crate names the commercial edition\'s crate, for --edition pro only')
crate=(arguments.crate or ROOT).resolve()
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
lock_sha=sha(crate/'Cargo.lock');receipt=ROOT.parents[1]/'deploy/.offline'/('INPUTS.json' if edition=='core' else f'INPUTS-{edition}.json')
if not (crate/'.vendor').is_dir():
    subprocess.run(['cargo','vendor','--locked','--offline','.vendor'],cwd=crate,check=True,stdout=subprocess.DEVNULL)
languages=tomllib.loads((ROOT/'.cargo/config.toml').read_text())['env']['TSLP_LANGUAGES'].split(',')
caches=[ROOT.parents[1]/'.tslp-root'/'parsers',*(ROOT/'target').glob('*/build/tree-sitter-language-pack-*/out/_parsers/parsers')]
source=next((c for c in caches if all((c/x/'src/parser.c').is_file() for x in languages) and (c.parent/'patches').is_dir()),None)
if source is None:raise SystemExit('Run tools/prepare_parser_sources.py first; no complete parser source folder exists.')
destination=ROOT.parents[1]/'deploy/.offline/parsers';destination.mkdir(parents=True,exist_ok=True)
files={}
for language in languages:
    shutil.copytree(source/language,destination/language,dirs_exist_ok=True)
    for p in sorted((destination/language).rglob('*')):
        if p.is_file():files[str(p.relative_to(destination))]=sha(p)
patches=destination.parent/'patches'
shutil.copytree(source.parent/'patches',patches,dirs_exist_ok=True)
patch_files={str(p.relative_to(patches)):sha(p) for p in sorted(patches.rglob('*')) if p.is_file()}
if not patch_files:raise SystemExit('Refusing a build without the upstream grammar memory-safety patches.')
receipt.write_text(json.dumps({'edition':edition,'cargo_lock_sha256':lock_sha,'parser_source':'pinned tree-sitter-language-pack 1.20.0 parser sources ('+('prepared .tslp-root' if source==caches[0] else 'build cache')+'); upstream safety patches remain enabled','languages':languages,'files':files,'patch_files':patch_files},indent=2)+'\n')
print(json.dumps({'edition':edition,'languages':len(languages),'files':len(files),'patch_files':len(patch_files),'cargo_lock_sha256':lock_sha,'copied_credentials':False}))
