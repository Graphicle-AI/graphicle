#!/usr/bin/env python3
"""Write the Homebrew formula for a release from its SHA256SUMS.

    python3 packaging/homebrew/fill_formula.py --version 0.1.0 --sums dist/SHA256SUMS --out graphicle.rb
    python3 packaging/homebrew/fill_formula.py --version 0.1.0 \\
        --sums https://github.com/graphicle-ai/graphicle/releases/download/v0.1.0/SHA256SUMS

SHA256SUMS has one "<sha256>  graphicle-<version>-<target>.tar.gz" line per
archive, as sha256sum writes it. Each of the four targets must appear exactly
once. The result replaces Formula/graphicle.rb in graphicle-ai/homebrew-tap.
"""
import argparse
import re
import sys
import urllib.request
from pathlib import Path

TEMPLATE = Path(__file__).resolve().with_name('graphicle.rb.in')
TARGETS = ['aarch64-apple-darwin', 'x86_64-apple-darwin', 'aarch64-unknown-linux-gnu', 'x86_64-unknown-linux-gnu']
VERSION = re.compile(r'\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?')
LINE = re.compile(r'([0-9a-f]{64}) [ *](\S+)')


def read_sums(source):
    if source.startswith('https://'):
        request = urllib.request.Request(source, headers={'User-Agent': 'graphicle-fill-formula'})
        with urllib.request.urlopen(request, timeout=60) as response:
            return response.read().decode('utf-8')
    if '://' in source:
        sys.exit(f'{source}: only https:// addresses or local files are read')
    return Path(source).read_text(encoding='utf-8')


def fill(template, version, sums_text):
    sums = {}
    for number, line in enumerate(sums_text.splitlines(), 1):
        if not line.strip():
            continue
        match = LINE.fullmatch(line.strip())
        if not match:
            sys.exit(f'SHA256SUMS line {number} is not "<sha256>  <file>": {line!r}')
        digest, name = match.groups()
        if name in sums and sums[name] != digest:
            sys.exit(f'SHA256SUMS lists {name} twice with different sums')
        sums[name] = digest
    text = template.replace('@VERSION@', version)
    for target in TARGETS:
        name = f'graphicle-{version}-{target}.tar.gz'
        if name not in sums:
            sys.exit(f'SHA256SUMS has no line for {name}')
        placeholder = f'@SHA256:{target}@'
        if text.count(placeholder) != 1:
            sys.exit(f'the template must hold {placeholder} exactly once')
        text = text.replace(placeholder, sums[name])
    left = re.findall(r'@[A-Z0-9_:a-z-]+@', text)
    if left:
        sys.exit('the template still has ' + ', '.join(sorted(set(left))))
    return text


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--version', required=True, help='the release version, without the tag\'s "v"')
    parser.add_argument('--sums', required=True, help='SHA256SUMS: a local file or an https:// address')
    parser.add_argument('--template', type=Path, default=TEMPLATE)
    parser.add_argument('--out', type=Path, help='where to write the formula (default: standard output)')
    args = parser.parse_args()
    if not VERSION.fullmatch(args.version):
        sys.exit(f'--version {args.version!r} is not a version such as 0.1.0')
    text = fill(args.template.read_text(encoding='utf-8'), args.version, read_sums(args.sums))
    if args.out:
        args.out.write_text(text, encoding='utf-8')
    else:
        sys.stdout.write(text)


if __name__ == '__main__':
    main()
