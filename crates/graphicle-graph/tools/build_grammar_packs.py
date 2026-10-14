#!/usr/bin/env python3
"""Build grammar packs: the long-tail tree-sitter grammars as shared libraries.

A build without the `grammars-extra` feature (or one `grammar-*` feature)
loads those grammars from packs at first use (src/grammar_packs.rs). Each pack
is compiled from the exact grammar crate the default build links, found via
`cargo metadata`, so both builds parse identically; the loader also refuses a
pack whose grammar has other node-kind/parse-state counts.

Usage:
  build_grammar_packs.py --out DIR [--cc cc] [--pack verilog ...]
Writes DIR/<pack>.<dylib|so|dll> and DIR/manifest.json (crate versions and
SHA-256 of every pack).
"""
import argparse, hashlib, json, platform, subprocess, sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
# pack -> (crate, grammar source dirs relative to the crate)
PACKS = {
    "verilog": ("tree-sitter-verilog", ["src"]),
    "ocaml": ("tree-sitter-ocaml", ["grammars/ocaml/src", "grammars/interface/src"]),
    "fortran": ("tree-sitter-fortran", ["src"]),
    "julia": ("tree-sitter-julia", ["src"]),
    "objc": ("tree-sitter-objc", ["src"]),
    "elixir": ("tree-sitter-elixir", ["src"]),
    "powershell": ("tree-sitter-powershell", ["src"]),
    "zig": ("tree-sitter-zig", ["src"]),
    "dm": ("tree-sitter-dm", ["src"]),
    "commonlisp": ("tree-sitter-commonlisp", ["src"]),
}


def extension():
    system = platform.system()
    return "dll" if system == "Windows" else "dylib" if system == "Darwin" else "so"


def crate_dirs():
    meta = json.loads(subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--manifest-path", str(ROOT / "Cargo.toml")],
        capture_output=True, text=True, check=True).stdout)
    return {p["name"]: (Path(p["manifest_path"]).parent, p["version"]) for p in meta["packages"]}


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument("--cc", default="cc")
    ap.add_argument("--pack", action="append", choices=sorted(PACKS))
    args = ap.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    crates = crate_dirs()
    manifest = {"packs": {}, "platform": f"{platform.system()}-{platform.machine()}"}
    for pack in args.pack or sorted(PACKS):
        crate, dirs = PACKS[pack]
        base, version = crates[crate]
        sources, includes = [], []
        for d in dirs:
            src = base / d
            includes += ["-I", str(src)]
            for name in ("parser.c", "scanner.c"):
                if (src / name).exists():
                    sources.append(str(src / name))
        target = args.out / f"{pack}.{extension()}"
        cmd = [args.cc, "-shared", "-fPIC", "-O2", "-std=c11", "-w", *includes, *sources, "-o", str(target)]
        subprocess.run(cmd, check=True)
        digest = hashlib.sha256(target.read_bytes()).hexdigest()
        manifest["packs"][pack] = {"crate": crate, "version": version, "file": target.name,
                                   "bytes": target.stat().st_size, "sha256": digest}
        print(f"{pack:<11} {crate} {version} -> {target.name} ({target.stat().st_size / 1e6:.1f} MB)")
    (args.out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")


if __name__ == "__main__":
    sys.exit(main())
