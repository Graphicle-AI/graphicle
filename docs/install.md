# Install Graphicle

Graphicle runs on macOS 11 or later (Apple silicon and Intel) and on Linux
(x86-64 and arm64, with glibc 2.35 or later, which covers Ubuntu 22.04,
Debian 12, Fedora 36 and newer). Windows: no native build yet; under WSL2 the
Linux install should work (not yet tested). Native Windows support is planned.

A release has two programs: `graphicle` and `graphicle-graph`, the code-graph
engine. Keep them in the same folder on your `PATH`.

After you install, set up a repository with
`cd your-repo && graphicle setup`. The [quick start](quickstart.md) shows each
step.

## Homebrew

```sh
brew install graphicle-ai/tap/graphicle
graphicle --version          # graphicle 0.1.0 (core)
```

`brew upgrade graphicle` installs a new release. Afterwards, read
[After an upgrade](#after-an-upgrade).

## Install script

The script picks the archive for your machine, checks its SHA-256 and copies
both programs into `~/.local/bin`. It never uses `sudo`.

```sh
curl -fsSL https://github.com/graphicle-ai/graphicle/releases/latest/download/install.sh | sh
```

The script takes these options:

- `--version 0.1.0` installs that release instead of the latest one.
- `--prefix DIR` installs into `DIR/bin`.
- `--bin-dir DIR` installs into `DIR` itself.

To pass an option through the pipe, put it after `sh -s --`:

```sh
curl -fsSL https://github.com/graphicle-ai/graphicle/releases/latest/download/install.sh | sh -s -- --version 0.1.0
```

If `~/.local/bin` is not on your `PATH`, add it, for example with
`export PATH="$HOME/.local/bin:$PATH"` in your shell profile.

## Release archives

Each release on the
[Releases page](https://github.com/graphicle-ai/graphicle/releases) has an
archive for each platform, `graphicle-<version>-<target>.tar.gz`, with a
`.sha256` file beside it (`SHA256SUMS` lists them all). Each archive holds a
folder with both programs, `LICENSE`, `NOTICE`, `NOTICE.md`, `README.md`,
`CHANGELOG.md` and the third-party licenses.

| Platform | Target |
|---|---|
| macOS, Apple silicon | `aarch64-apple-darwin` |
| macOS, Intel | `x86_64-apple-darwin` |
| Linux, x86-64 | `x86_64-unknown-linux-gnu` |
| Linux, arm64 | `aarch64-unknown-linux-gnu` |

```sh
version=0.1.0
target=aarch64-apple-darwin
base=https://github.com/graphicle-ai/graphicle/releases/download/v$version
curl -fLO "$base/graphicle-$version-$target.tar.gz"
curl -fLO "$base/graphicle-$version-$target.tar.gz.sha256"
shasum -a 256 -c "graphicle-$version-$target.tar.gz.sha256"   # on Linux: sha256sum -c …
tar -xzf "graphicle-$version-$target.tar.gz"
mkdir -p ~/.local/bin
cp "graphicle-$version-$target/graphicle" "graphicle-$version-$target/graphicle-graph" ~/.local/bin/
```

If you downloaded an archive with a web browser and macOS will not open the
programs, download it again with `curl` as shown above.

## Build from source

You need:

- Rust 1.94.1 or newer (`rustup` installs it);
- a C compiler: the Xcode Command Line Tools on macOS, or `build-essential`
  (or your distribution's equivalent) on Linux;
- Python 3, to prepare the parser sources. Unpacking them needs Python 3.14
  or newer, the `zstandard` module or the `zstd` program;
- about 5 GB of free disk. Each program's release build takes roughly ten
  minutes on a laptop.

```sh
git clone https://github.com/graphicle-ai/graphicle
cd graphicle
python3 crates/graphicle/tools/prepare_parser_sources.py   # once per clone
(cd crates/graphicle && cargo build --release --locked)
(cd crates/graphicle-graph && cargo build --release --locked -p graphicle-graph)
```

Copy both programs, `crates/graphicle/target/release/graphicle` and
`crates/graphicle-graph/target/release/graphicle-graph`, into one folder on
your `PATH`.

`prepare_parser_sources.py` downloads the grammar pack's parser sources once
(a 36.6 MB bundle, pinned by SHA-256) and keeps about 540 MB of it in
`.tslp-root/` at the repository root. After that, builds need no network
access. Run the script again to check the folder. Build `crates/graphicle/`
from inside its folder, as shown, so that Cargo reads
`crates/graphicle/.cargo/config.toml`. Without that file the grammar pack
downloads and unpacks about 2 GB on every build.

Optional features:

| Feature | Build | Adds |
|---|---|---|
| `local-inference` | `cargo build --release --locked --features local-inference` in `crates/graphicle/` | Local ONNX embedding and reranking ([crates/graphicle/LOCAL_INFERENCE.md](../crates/graphicle/LOCAL_INFERENCE.md)) |
| `bedrock` | `cargo build --release --locked -p graphicle-graph --features bedrock` in `crates/graphicle-graph/` | The Amazon Bedrock backend for semantic extraction |
| `transcription` | `… --features transcription` in `crates/graphicle-graph/` | Local audio and video transcription for extraction. It needs CMake to build and FFmpeg and model weights to run |
| `postgres` | `… --features postgres` in `crates/graphicle-graph/` | Extraction of PostgreSQL catalogs (`extract --postgres DSN`). It links libpq |

Release builds use the default features, and the four features above are not
in them.

## Check the installation

```sh
graphicle --version          # graphicle 0.1.0 (core)
graphicle graph --help       # the code-graph engine's commands
graphicle doctor             # checks the programs, the data folder and your agents
```

The engine also runs on its own as `graphicle-graph`.

## After an upgrade

Agent entries and hooks start the program through the link on your `PATH`
(Homebrew's `bin/graphicle`, for example), which an upgrade keeps. After an
upgrade, check with:

```sh
graphicle doctor --fix
```

It repairs agent entries and capture hooks that start a program that no
longer exists, such as the ones an older Graphicle wrote with a versioned
path. For
anything else it reports, run the command it prints. If you installed the
graph's agent rules or Git hooks, also run `graphicle graph <platform> install`
or `graphicle graph hook install` again. `graphicle update` says how to
upgrade for the way you installed Graphicle.

## Uninstall

1. Remove what Graphicle added to your agents and repositories:

   ```sh
   graphicle uninstall
   ```

   It removes Graphicle's agent entries, capture hooks, instruction sections
   and Git-hook lines everywhere, then tells you how to remove the programs.
   `graphicle uninstall --purge` also deletes the data folder, after you
   confirm.
2. If you installed them, remove the graph's skills and rules with
   `graphicle graph uninstall`, and its Git hooks with
   `graphicle graph hook uninstall` in each repository.
3. Remove the programs: run `brew uninstall graphicle`, or delete `graphicle`
   and `graphicle-graph` from the folder you installed them in.
4. To delete your data by hand, remove the following:
   - the data folder: `~/Library/Application Support/Graphicle` on macOS, or
     `~/.local/share/graphicle` on Linux (`$XDG_DATA_HOME/graphicle` when
     that variable is set);
   - the engine's settings folder, `~/.graphicle-graph/`;
   - the `.graphicle/` folder in each project, and the `.graphicle-bak`
     backups that setup made.
