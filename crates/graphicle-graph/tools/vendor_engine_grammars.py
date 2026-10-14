#!/usr/bin/env python3
"""Retain generated C dependencies when the matching Rust release is unavailable."""
import os
import hashlib,io,json,tarfile,urllib.request
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
SPECS={
    "kotlin":("1.1.0","https://files.pythonhosted.org/packages/58/bb/bdab3665eeca21246130eec79c76e42456cfa72d59606266ecdbf37f9a96/tree_sitter_kotlin-1.1.0.tar.gz","322a35bdae75e25ae64dae6027be609c5422fab282084117816c4ebcda6168da","tree_sitter_kotlin-1.1.0.tar.gz"),
    "php":("0.24.1","https://codeload.github.com/tree-sitter/tree-sitter-php/tar.gz/refs/tags/v0.24.1","3320ba539ea988967a02568dbf41945df0044ecb9fa88ea38daa1116489093db","tree-sitter-php-0.24.1-git.tar.gz"),
}
def archive(url,sha,filename):
    cached=Path("/tmp/graphify-parser-sources")/filename
    data=cached.read_bytes() if cached.exists() else urllib.request.urlopen(url,timeout=30).read()
    assert hashlib.sha256(data).hexdigest()==sha
    return tarfile.open(fileobj=io.BytesIO(data))
def save(root,name,blob,files):
    assert ".." not in Path(name).parts and not Path(name).is_absolute()
    path=root/name;path.parent.mkdir(parents=True,exist_ok=True);path.write_bytes(blob)
    files[name]=hashlib.sha256(blob).hexdigest()
for lang,(version,url,sha,filename) in SPECS.items():
    target=ROOT/"vendor"/("tree-sitter-"+lang);files={}
    tar=archive(url,sha,filename)
    for entry in tar.getmembers():
        relative="/".join(Path(entry.name).parts[1:])
        if entry.isfile() and (relative=="LICENSE" or relative.startswith(("src/","php/src/","php_only/src/","common/")) and relative.endswith((".c",".h"))):
            save(target,relative,tar.extractfile(entry).read(),files)
    extra={}
    if lang=="kotlin":
        # Its sdist omits the public tree-sitter C headers. Use the compatible
        # ABI headers in the already pinned MIT-licensed Java parser package.
        headers=next((Path(os.environ.get("CARGO_HOME",Path.home()/".cargo"))/"registry/src").glob("*/tree-sitter-java-0.23.5/src/tree_sitter"))
        for path in headers.glob("*.h"):save(target,"src/tree_sitter/"+path.name,path.read_bytes(),files)
        extra={"header_source":"tree-sitter-java 0.23.5 (MIT); Kotlin sdist omits these public ABI headers"}
    (target/"SOURCE.json").write_text(json.dumps({"url":url,"sha256":sha,"files":files,**extra},indent=2)+"\n")
# Groovy's generated parser is compared directly with its Python sdist.
target=ROOT/"vendor/tree-sitter-groovy";files={}
path=Path("/tmp/graphify-parser-sources/tree-sitter-groovy-0.1.2-git.tar.gz")
data=path.read_bytes();tar=tarfile.open(fileobj=io.BytesIO(data))
source_tar=archive("https://files.pythonhosted.org/packages/e8/1f/400d296618ea95932e6a3d299eababda0d138f4b0cfeaacdf50601c40ca9/tree_sitter_groovy-0.1.2.tar.gz","49b004c4ae946d3f01a602f325cd8996423e034e5b3ad36fc34a1d1e42afa8da","tree_sitter_groovy-0.1.2.tar.gz")
parser_source=source_tar.extractfile("tree_sitter_groovy-0.1.2/src/parser.c").read()
for entry in tar.getmembers():
    relative="/".join(Path(entry.name).parts[1:])
    if entry.isfile() and relative.startswith("src/") and relative.endswith((".c",".h")):
        blob=tar.extractfile(entry).read()
        if relative=="src/parser.c":assert blob==parser_source
        save(target,relative,blob,files)
    elif relative=="Cargo.toml":
        save(target,"UPSTREAM-LICENSE-DECLARATION.toml",tar.extractfile(entry).read(),files)
(target/"SOURCE.json").write_text(json.dumps({"url":"https://codeload.github.com/amaanq/tree-sitter-groovy/tar.gz/refs/tags/v0.1.2","sha256":hashlib.sha256(data).hexdigest(),"python_sdist_sha256":"49b004c4ae946d3f01a602f325cd8996423e034e5b3ad36fc34a1d1e42afa8da","parser_matches_python_sdist":True,"license":"MIT declared in upstream Cargo.toml; no standalone license file exists in the tag or sdist","files":files},indent=2)+"\n")
print("Retained Kotlin, PHP and Groovy native grammar sources with checksums.")
