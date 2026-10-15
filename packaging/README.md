# Releasing Graphicle

A release is a version tag. `.github/workflows/release.yml` builds both
programs, `graphicle` and `graphicle-graph`, on a native runner for each
target, packages them with `release_build.py` and publishes the GitHub release.

| Target | Runner | Runs on |
|---|---|---|
| `aarch64-apple-darwin` | macos-15 | macOS 11 or later, Apple silicon |
| `x86_64-apple-darwin` | macos-15-intel | macOS 11 or later, Intel |
| `x86_64-unknown-linux-gnu` | ubuntu-22.04 | Linux with glibc 2.35 or later |
| `aarch64-unknown-linux-gnu` | ubuntu-22.04-arm | Linux with glibc 2.35 or later |
| `x86_64-pc-windows-msvc` | windows-2022 | Windows on x86-64 (preview) |

Each archive, `graphicle-<version>-<target>.tar.gz`, holds a folder of the same
name with both programs, `LICENSE`, `NOTICE`, `README.md`, `CHANGELOG.md`,
`NOTICE.md` and `licenses/`. The release also carries a `.sha256` file per archive,
`SHA256SUMS`, `install.sh`, `install.ps1` and the Homebrew formula `graphicle.rb`.

For Windows the archive is a `.zip` (`graphicle-<version>-x86_64-pc-windows-msvc.zip`)
with `graphicle.exe` and `graphicle-graph.exe`. The workflow builds them with
`RUSTFLAGS="-C target-feature=+crt-static"`, so they need no Visual C++ runtime,
and `release_build.py` fails if a program imports anything but Windows system
libraries. `python3 packaging/test_release_build.py` tests that check and the
`.zip` writer on any system.

## Steps

1. Set the version in `crates/graphicle/Cargo.toml` (and its `Cargo.lock` entry):
   `graphicle --version` must print it. Add its section to `CHANGELOG.md`
   with the release date (`## 0.1.0 - 2026-10-14`); the release notes are that
   section.
2. Rehearse with a prerelease tag. It builds and publishes a prerelease, which
   neither `releases/latest` nor the Homebrew tap picks up:

   ```sh
   git tag -s v0.1.0-rc.1 -m "Graphicle 0.1.0-rc.1" && git push origin v0.1.0-rc.1
   curl -fsSL https://github.com/graphicle-ai/graphicle/releases/download/v0.1.0-rc.1/install.sh | sh -s -- --version 0.1.0-rc.1 --bin-dir /tmp/graphicle-rc
   /tmp/graphicle-rc/graphicle --version
   ```

3. Release: `git tag -s v0.1.0 -m "Graphicle 0.1.0" && git push origin v0.1.0`.
4. Homebrew: with the repository secret `HOMEBREW_TAP_TOKEN` (a fine-grained
   token with contents read and write on `graphicle-ai/homebrew-tap`, which
   needs at least one commit, such as a README), the workflow commits the new
   `Formula/graphicle.rb` to the tap. Without it,
   copy `graphicle.rb` from the release to the tap's `Formula/` folder, or
   write it from the published sums:

   ```sh
   python3 packaging/homebrew/fill_formula.py --version 0.1.0 \
     --sums https://github.com/graphicle-ai/graphicle/releases/download/v0.1.0/SHA256SUMS \
     --out ../homebrew-tap/Formula/graphicle.rb
   ```

5. Check: `brew install graphicle-ai/tap/graphicle && brew test graphicle`, and
   `curl -fsSL https://github.com/graphicle-ai/graphicle/releases/latest/download/install.sh | sh`.

Running the workflow by hand (Actions, release, Run workflow) builds and
packages every target without releasing; the archives are a workflow artifact.

## Locally

```sh
python3 crates/graphicle/tools/prepare_parser_sources.py   # once per checkout
python3 packaging/release_build.py --out dist               # this machine's target
sh install.sh --base-url "file://$PWD/dist" --version <version> --bin-dir /tmp/graphicle-test
```

A local build is for testing; publish only the archives the release workflow
builds. `release_build.py` replaces the checkout, Cargo's home, the build
folder and the home folder in the paths Rust code embeds, but C code compiled
by build scripts can still name the build machine's folders.
