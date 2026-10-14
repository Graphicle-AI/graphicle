#!/usr/bin/env python3
"""Build a standalone Rust CLI and a local macOS app/DMG of one edition. Never publish.

--edition core builds this crate (Graphicle Core). --edition pro builds the
`graphicle` program of the crate given with --crate (the commercial edition),
whose own documents are added with --document. Both editions install as the same
application (one bundle id), so installing it replaces Core in place; their
disk images and receipts carry the edition, and the binary must report it
(`graphicle <version> (<edition>)`).
"""
import argparse
import hashlib
import json
from pathlib import Path
import platform
import plistlib
import shutil
import subprocess
import tempfile
import time
import tomllib

ROOT = Path(__file__).resolve().parents[1]
EDITIONS = ["core", "pro"]
# The notices every app bundle carries (paths relative to ROOT), and the
# linked graph engine's notices (relative to ROOT.parent, the crates folder).
NOTICES = ["LICENSE", "NOTICE", "NOTICE.md", "UPSTREAM.json", "GRAMMAR_NOTICES.json", "THIRD_PARTY_LICENSES.html", "README.md"]
ENGINE_NOTICES = ["graphicle-graph/NOTICE", "graphicle-graph/NOTICE.md", "graphicle-memory/NOTICE.md"]


def run(*arguments):
    subprocess.run(list(map(str, arguments)), cwd=ROOT, check=True)


def fingerprint(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def atomic_copy(source, destination):
    # Replacing a Mach-O executable in place can invalidate an active mapping
    # or the macOS signature cache. A fresh inode preserves running versions.
    destination = Path(destination)
    temporary = destination.with_name(f".{destination.name}.next")
    if temporary.exists(): temporary.unlink()
    shutil.copy2(source, temporary)
    temporary.replace(destination)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--edition", choices=EDITIONS, required=True)
    parser.add_argument("--crate", type=Path, help="Crate that builds the edition's `graphicle` program (default and only choice for core: this crate)")
    parser.add_argument("--document", type=Path, action="append", default=[], help="Another document for the app bundle (repeatable)")
    parser.add_argument("--dmg", action="store_true")
    parser.add_argument("--binary", type=Path, help="Package an already-built service executable; skips Cargo and records its hash")
    parser.add_argument("--sign-identity", help="Installed Apple Developer ID Application identity")
    parser.add_argument("--notary-profile", help="Existing notarytool keychain profile; requires --dmg and --sign-identity")
    args = parser.parse_args()
    if args.notary_profile and not (args.dmg and args.sign_identity):
        parser.error("Notarization requires --dmg and --sign-identity")
    if args.edition == "core" and args.crate and args.crate.resolve() != ROOT:
        parser.error("Graphicle Core is this crate")
    if args.edition == "pro" and not (args.crate or args.binary):
        parser.error("--edition pro needs that edition's crate (--crate) or its built binary (--binary)")
    crate = (args.crate or ROOT).resolve()
    documents = [ROOT / name for name in NOTICES] + [path.resolve() for path in args.document]
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    expected_version = f"graphicle {version} ({args.edition})"
    def sign(path):
        extra = ["--options", "runtime", "--timestamp"] if args.sign_identity else []
        run("codesign", "--force", "--sign", args.sign_identity or "-", *extra, path)
    if args.binary:
        source_binary = args.binary.resolve()
        print("Packaging the supplied Rust service; Cargo compilation is not repeated.", flush=True)
    else:
        print(f"Building the standalone Rust service ({args.edition}).", flush=True)
        subprocess.run(["cargo", "build", "--release", "--locked", "--bin", "graphicle"], cwd=crate, check=True)
        metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--offline", "--locked", "--no-deps", "--format-version", "1"],
                                                      cwd=crate, text=True))
        source_binary = Path(metadata["target_directory"]) / "release/graphicle"
    reported_version = subprocess.check_output([str(source_binary), "--version"], text=True).strip()
    if reported_version != expected_version:
        parser.error(f"Expected {expected_version}; the service binary reports {reported_version!r}")
    source_binary_sha256 = fingerprint(source_binary)
    dist = ROOT / "dist"
    dist.mkdir(exist_ok=True)
    binary = dist / "graphicle"
    atomic_copy(source_binary, binary)
    run(binary, "--version")
    artifacts = [binary]
    if platform.system() == "Darwin":
        print("Building the macOS application shell and icon.", flush=True)
        app = dist / "Graphicle.app"
        if app.exists():
            shutil.rmtree(app)  # owned build output only; never ~/Applications
        contents = app / "Contents"
        macos = contents / "MacOS"
        resources = contents / "Resources"
        macos.mkdir(parents=True)
        resources.mkdir()
        shutil.copy2(binary, resources / "graphicle")
        swift_arch = "arm64" if platform.machine() in {"arm64", "aarch64"} else "x86_64"
        run("swiftc", "-O", "-parse-as-library", "-target", f"{swift_arch}-apple-macos13.0",
            "../../desktop/Graphicle.swift", "-o", macos / "Graphicle", "-framework", "AppKit")
        run("swiftc", "-O", "-target", f"{swift_arch}-apple-macos13.0", "../../desktop/DocumentHelper.swift",
            "-o", resources / "graphicle-document", "-framework", "Vision", "-framework", "PDFKit", "-framework", "AppKit")
        with tempfile.TemporaryDirectory(prefix="graphicle-icon-") as temp:
            temp = Path(temp)
            run("swiftc", "../../desktop/Icon.swift", "-o", temp / "draw-icon", "-framework", "AppKit")
            run(temp / "draw-icon", temp / "icon.png")
            iconset = temp / "Graphicle.iconset"
            iconset.mkdir()
            for size in (16, 32, 128, 256, 512):
                for scale in (1, 2):
                    name = f"icon_{size}x{size}{'@2x' if scale == 2 else ''}.png"
                    subprocess.run(["sips", "-z", str(size * scale), str(size * scale), str(temp / "icon.png"), "--out", str(iconset / name)], check=True, stdout=subprocess.DEVNULL)
            run("iconutil", "-c", "icns", iconset, "-o", resources / "Graphicle.icns")
        metadata = {"CFBundleName": "Graphicle", "CFBundleDisplayName": "Graphicle", "CFBundleExecutable": "Graphicle",
                    "CFBundleIdentifier": "com.karlexai.graphicle.native", "CFBundleShortVersionString": version, "CFBundleVersion": version,
                    "CFBundlePackageType": "APPL", "CFBundleIconFile": "Graphicle.icns", "LSMinimumSystemVersion": "13.0",
                    "NSHighResolutionCapable": True,
                    "NSHumanReadableCopyright": "Copyright (c) 2026 KarLex AI, Inc. Licensed under the Apache License 2.0." if args.edition == "core"
                    else "Copyright (c) 2026 KarLex AI, Inc. This edition is commercial software and includes Apache-2.0 components."}
        (contents / "Info.plist").write_bytes(plistlib.dumps(metadata))
        for source in documents:
            if not source.is_file():
                raise SystemExit(f"Missing release document: {source}")
            shutil.copy2(source, resources / source.name)
        shutil.copytree(ROOT / "licenses", resources / "licenses")
        for name in ENGINE_NOTICES:
            (resources / name).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT.parent / name, resources / name)
        shutil.copytree(ROOT.parent / "graphicle-graph/licenses", resources / "graphicle-graph/licenses")
        sign(resources / "graphicle")
        sign(resources / "graphicle-document")
        sign(binary)
        shutil.copy2(resources / "graphicle-document", dist / "graphicle-document")
        sign(app)
        run("codesign", "--verify", "--deep", "--strict", app)
        run(resources / "graphicle", "--version")
        artifacts.extend([macos / "Graphicle", resources / "graphicle", resources / "graphicle-document", resources / "Graphicle.icns", dist / "graphicle-document"])
        if args.dmg:
            print("Creating the local disk image.", flush=True)
            with tempfile.TemporaryDirectory(prefix="graphicle-dmg-") as temp:
                stage = Path(temp)
                shutil.copytree(app, stage / app.name)
                (stage / "Applications").symlink_to("/Applications", target_is_directory=True)
                # Graphicle-<version>-macos-<arch>.dmg for Core; other editions add their name.
                product = "Graphicle" + ("" if args.edition == "core" else "-" + args.edition.title())
                dmg = dist / f"{product}-{version}-macos-{platform.machine()}.dmg"
                if dmg.exists():
                    dmg.unlink()
                run("hdiutil", "create", "-volname", product.replace("-", " "), "-srcfolder", stage, "-format", "UDZO", dmg)
                run("hdiutil", "verify", dmg)
                if args.sign_identity:
                    sign(dmg)
                if args.notary_profile:
                    run("xcrun", "notarytool", "submit", dmg, "--keychain-profile", args.notary_profile, "--wait")
                    run("xcrun", "stapler", "staple", dmg)
                    run("xcrun", "stapler", "validate", dmg)
                artifacts.append(dmg)
    receipt = {"version": version, "edition": args.edition, "verified_binary_version": reported_version, "created_unix": time.time(), "platform": platform.platform(), "architecture": platform.machine(),
               "python_required_by_runtime": False, "model_weights_bundled": False, "published": False,
               "service_compiled_in_this_packaging_run": not bool(args.binary),
               "source_binary_sha256": source_binary_sha256,
               "macos_signing": "Developer ID" if args.sign_identity else "local ad-hoc; no Developer ID or notarization", "notarized": bool(args.notary_profile), "cargo_lock_sha256": fingerprint(crate / "Cargo.lock") if args.crate or args.edition == "core" else None,
               "artifacts": [{"path": str(path.relative_to(ROOT)), "bytes": path.stat().st_size, "sha256": fingerprint(path)} for path in artifacts]}
    (dist / "ARTIFACTS.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(f"Build ready in {dist}", flush=True)


if __name__ == "__main__":
    main()
