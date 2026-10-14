#!/usr/bin/env python3
"""Package the already-built Linux image's binary and notices of one edition. Never publish.

--edition core packages the Graphicle Core image (graphicle:<version>, built
from deploy/Dockerfile); --edition pro packages the commercial edition's
image (graphicle-pro:<version>, built from its crate), with its documents
given by --document. The image's binary must report the edition.
"""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import tomllib
import uuid

ROOT = Path(__file__).resolve().parents[1]
EDITIONS = ['core', 'pro']
# Documents every archive ships next to the image's notices (relative to ROOT).
DOCUMENTS = ['README.md', 'UPSTREAM.json', 'GRAMMAR_NOTICES.json']


def docker(*args):
    return subprocess.check_output(['docker', *map(str, args)], text=True).strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    version = tomllib.loads((ROOT/'Cargo.toml').read_text())['package']['version']
    parser.add_argument('--edition', choices=EDITIONS, required=True)
    parser.add_argument('--image', help="Default: the edition's image tag for this version")
    parser.add_argument('--document', type=Path, action='append', default=[], help='Another document to ship (repeatable)')
    parser.add_argument('--output-dir', type=Path, default=ROOT/'dist')
    args = parser.parse_args()
    # Names carry the edition after Core's: graphicle:<version> and
    # Graphicle-<version>-linux-<arch> for Core; other editions add their name
    # to both.
    suffix = '' if args.edition == 'core' else '-' + args.edition
    product = 'Graphicle' + suffix.title()
    args.image = args.image or f'graphicle{suffix}:{version}'
    expected_version = f'graphicle {version} ({args.edition})'
    image = json.loads(docker('image', 'inspect', args.image))[0]
    if image['Os'] != 'linux':
        parser.error('A Linux image is required')
    architecture = {'arm64': 'aarch64', 'amd64': 'x86_64'}.get(image['Architecture'], image['Architecture'])
    release = f'{product}-{version}-linux-{architecture}'
    name = 'graphicle-package-' + uuid.uuid4().hex[:10]
    args.output_dir.mkdir(parents=True, exist_ok=True)
    archive = args.output_dir/(release + '.tar.gz')
    try:
        docker('create', '--name', name, args.image, '--version')
        actual_version = docker('start', '-a', name)
        if actual_version != expected_version:
            raise ValueError(f'Image reports {actual_version!r}; refusing to label it {expected_version!r}')
        with tempfile.TemporaryDirectory(prefix='graphicle-linux-release-') as temp:
            root = Path(temp)/release
            (root/'bin').mkdir(parents=True)
            (root/'share').mkdir()
            docker('cp', name + ':/usr/local/bin/graphicle', root/'bin/graphicle')
            docker('cp', name + ':/usr/share/graphicle', root/'share/graphicle')
            for file in [ROOT/document for document in DOCUMENTS] + [path.resolve() for path in args.document]:
                shutil.copy2(file, root/'share/graphicle'/file.name)
            (root/'INSTALL.txt').write_text(product.replace('-', ' ')+' '+version+' for Linux '+architecture+'\n\n'
                'Requires a Debian 12-compatible glibc environment.\n'
                'Run bin/graphicle --version, then bin/graphicle sync /path/to/project.\n'
                'Run bin/graphicle start for the local browser app.\n'
                'Keep the share/graphicle notices with this distribution.\n'
                'The Linux build does not bundle macOS Vision OCR or model weights.\n'
                'This locally built artifact is not a publicly signed release.\n')
            binary_sha = hashlib.sha256((root/'bin/graphicle').read_bytes()).hexdigest()
            temporary = archive.with_suffix(archive.suffix+'.tmp')
            with tarfile.open(temporary, 'w:gz') as tar:
                tar.add(root, arcname=release)
            temporary.replace(archive)
        receipt = {'version': version, 'edition': args.edition, 'verified_binary_version': actual_version,
            'platform': 'linux-'+architecture, 'image_id': image['Id'],
            'artifact': str(archive), 'bytes': archive.stat().st_size,
            'sha256': hashlib.sha256(archive.read_bytes()).hexdigest(), 'binary_sha256': binary_sha,
            'python_required_by_runtime': False, 'model_weights_bundled': False,
            'published': False, 'publisher_signed': False}
        (args.output_dir/'LINUX_ARTIFACTS.json').write_text(json.dumps(receipt, indent=2)+'\n')
        print(json.dumps(receipt, indent=2))
    finally:
        subprocess.run(['docker', 'rm', '-v', name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


if __name__ == '__main__':
    main()
