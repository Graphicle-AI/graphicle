# Install Graphicle

Graphicle runs on macOS 11 or later (Apple silicon and Intel), on Linux
(x86-64 and arm64, with glibc 2.35 or later, which covers Ubuntu 22.04,
Debian 12, Fedora 36 and newer) and, as a preview, on Windows (x86-64): see
[Windows (preview)](#windows-preview).

A release has two programs: `graphicle` and `graphicle-graph`, the code-graph
engine. Keep them in the same folder on your `PATH`.

After you install, set up a repository with
`cd your-repo && graphicle setup`. The [quick start](quickstart.md) shows each
step.

Claude Code's sandbox does not let setup write `.git/hooks`, `.mcp.json` or
`.claude/settings.local.json`, and Codex's sandbox keeps `.git` read-only.
There, setup prints the lines to add by hand and the command that adds the
agent's hooks later. Or run `graphicle setup` in a normal terminal.

## Homebrew

```sh
brew install graphicle-ai/tap/graphicle
graphicle --version          # graphicle 0.1.9 (core)
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

## Windows (preview)

On Windows (x86-64), install from PowerShell (Windows PowerShell 5.1 or
PowerShell 7):

```powershell
irm https://github.com/graphicle-ai/graphicle/releases/latest/download/install.ps1 | iex
```

The script downloads `graphicle-<version>-x86_64-pc-windows-msvc.zip`, checks
its SHA-256 before installing anything, copies `graphicle.exe` and
`graphicle-graph.exe` into `%LOCALAPPDATA%\Programs\Graphicle\bin` and adds
that folder to your user `PATH`. It needs no administrator rights. Open a new
terminal afterwards so that it sees the new `PATH`. Running the script again
upgrades the programs, even while they run.

To choose a release or a folder, set `GRAPHICLE_VERSION`,
`GRAPHICLE_INSTALL_DIR` or `GRAPHICLE_BASE_URL` before that line, for example
`$env:GRAPHICLE_VERSION = '<version>'`, or save the script and run it with
`-Version`, `-InstallDir`, `-BaseUrl` or `-NoModifyPath` (`-Help` lists them).
Windows runs a saved script only where its execution policy allows scripts,
so start a saved copy with
`powershell -ExecutionPolicy Bypass -File .\install.ps1 -Version <version>`.

On Windows, Graphicle keeps its data in `%LOCALAPPDATA%\Graphicle` and the
engine's settings in `%USERPROFILE%\.graphicle-graph`. The programs link the
C runtime statically, so they need no Visual C++ runtime.

Notes for the preview:

- Git hooks need [Git for Windows](https://gitforwindows.org/), whose `sh`
  runs them.
- Claude Code runs its hooks with Git Bash, or with PowerShell when Git for
  Windows is missing. If the path to `graphicle.exe` has a space or a
  non-ASCII character, keep its folder on `PATH` (`install.ps1` adds it)
  when Graphicle sets up Claude Code, so the hooks name `graphicle` and run
  in both shells; otherwise they need Git for Windows.
- For the local server, use `127.0.0.1` rather than `localhost`: the server
  listens on `127.0.0.1`, and Windows tries `localhost`'s IPv6 address first,
  which adds about two seconds to each request.
- `graphicle offboard` removes a project from the store and its graph cache
  but leaves the backups in the data folder's `backups/` folder as they are,
  and lists them as refused: Windows does not tell Graphicle whether a backup
  file has another name (a hard link) outside the data folder.
- Not on Windows: `graphicle-graph serve --socket` and
  `GRAPHICLE_REBUILD_MEMORY_LIMIT_MB`, which need Unix; OCR of scanned PDFs and
  images and the legacy document formats, which use a macOS helper; the
  desktop app, which is for macOS; and ARM64 Windows.
- The optional features under [Build from source](#build-from-source) are
  untested on Windows.

### Windows signing

Release builds are not yet signed with a trusted certificate. The release
workflow has `graphicle.exe` and `graphicle-graph.exe` signed through SignPath
with a test certificate, which Windows does not trust, so Windows treats them
as unsigned programs and may warn before it runs them (see **SmartScreen or
Smart App Control** below). When that signing fails, the release ships the
programs unsigned; each release's notes say which. Signing with the SignPath
Foundation's certificate, which Windows trusts, is planned. The
[Code signing policy](code-signing.md) says what is signed, who approves it,
and when Graphicle contacts the network.

### Troubleshooting on Windows

- **SmartScreen or Smart App Control.** The preview programs are not signed
  with a trusted certificate yet. `install.ps1` downloads with PowerShell,
  which SmartScreen does not check; a zip downloaded with a web browser can
  show "Windows protected your PC": choose **More info**, then **Run anyway**,
  or run `Unblock-File` on the zip before you unpack it. Windows 11's Smart
  App Control, when it is on, can block programs that lack a trusted
  signature; use WSL2 on such a machine until programs signed with a trusted
  certificate ship.
- **Microsoft Defender.** Defender scans each new program when it first runs,
  so the first start can take a few seconds. For builds from source, a
  [Dev Drive](https://learn.microsoft.com/windows/dev-drive/) keeps scanning
  out of the way.
- **Long paths.** If Git or a build reports a path that is too long, run
  `git config --global core.longpaths true`, and turn on Windows' long paths
  (as an administrator: `New-ItemProperty -Path
  HKLM:\SYSTEM\CurrentControlSet\Control\FileSystem -Name LongPathsEnabled
  -Value 1 -PropertyType DWORD -Force`).
- **Line endings.** Git for Windows usually checks out text files with CRLF
  line endings (`core.autocrlf=true`). Graphicle's own repository keeps LF
  endings on every system (its `.gitattributes` sets them), so a build from
  source needs no setting. If your team works on the same repositories from
  Windows and from macOS or Linux, `git config --global core.autocrlf input`
  keeps LF endings in the files Git checks out on Windows too, so that
  Graphicle sees the same file contents on every system.

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
| Windows, x86-64 (preview) | `x86_64-pc-windows-msvc`, a `.zip` |

```sh
version=0.1.9
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

On Windows, the archive is `graphicle-<version>-x86_64-pc-windows-msvc.zip`.
In PowerShell, compare `(Get-FileHash <archive>.zip).Hash` with the sum in its
`.sha256` file, unpack it with `Expand-Archive`, and copy `graphicle.exe` and
`graphicle-graph.exe` into one folder on your `PATH`.

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

On Windows, you need:

- Visual Studio 2022 Build Tools with the "Desktop development with C++"
  workload (the MSVC compiler and the Windows SDK). CMake, NASM, Perl and
  clang are not needed;
- Rust 1.94.1 or newer for the MSVC host, which `rustup` installs by default;
- Python 3.14 or newer, or an older Python 3 with
  `py -m pip install zstandard`;
- Git for Windows;
- long paths turned on in Windows and in Git (see **Long paths** under
  [Troubleshooting on Windows](#troubleshooting-on-windows));
- about 5 GB of free disk per crate, plus 0.5 GB for the parser sources.

```powershell
git clone -c core.longpaths=true https://github.com/graphicle-ai/graphicle
cd graphicle
py -3 crates\graphicle\tools\prepare_parser_sources.py   # once per clone
Push-Location crates\graphicle; cargo build --release --locked; Pop-Location
Push-Location crates\graphicle-graph; cargo build --release --locked -p graphicle-graph; Pop-Location
```

The programs are `crates\graphicle\target\release\graphicle.exe` and
`crates\graphicle-graph\target\release\graphicle-graph.exe`. Keep one target
folder per crate, as above: with one `CARGO_TARGET_DIR` for all three crates,
whose lockfiles differ, `graphicle-memory`'s tests fail to build. To build
programs that run without the Visual C++ runtime, as the release does, first
set `$env:RUSTFLAGS = '-C target-feature=+crt-static'`.

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
graphicle --version          # graphicle 0.1.9 (core)
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
upgrade for the way you installed Graphicle. On Windows, run the PowerShell
install line from [Windows (preview)](#windows-preview) again.

Upgrading from 0.1.0: run `graphicle setup` again in each repository. It
switches Claude Code to 0.1.1's lighter defaults: five tools and no guard
hooks. To keep 0.1.0's setup, with every tool and the guard hooks, run
`graphicle setup --tools all --guards` instead; the
[CHANGELOG](../CHANGELOG.md) lists the two small differences that remain.

You do not have to watch for releases: at most once a day, when you run a
command in a terminal, `graphicle` asks GitHub for the latest release. When it is
newer, `graphicle` prints the version and the upgrade command for your install.
`GRAPHICLE_NO_UPDATE_NOTIFIER=1` or `graphicle config set update_notifier false`
turns this off. The [privacy policy](code-signing.md#privacy-policy) lists
every case in which Graphicle contacts the network.

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
   and `graphicle-graph` from the folder you installed them in. On Windows,
   delete `graphicle.exe` and `graphicle-graph.exe` from
   `%LOCALAPPDATA%\Programs\Graphicle\bin` and remove that folder from your
   user `PATH` (Settings, search for "environment variables", then "Edit
   environment variables for your account").
4. To delete your data by hand, remove the following:
   - the data folder: `~/Library/Application Support/Graphicle` on macOS,
     `~/.local/share/graphicle` on Linux (`$XDG_DATA_HOME/graphicle` when
     that variable is set), or `%LOCALAPPDATA%\Graphicle` on Windows;
   - the engine's settings folder, `~/.graphicle-graph/`
     (`%USERPROFILE%\.graphicle-graph` on Windows);
   - the `.graphicle/` folder in each project, and the `.graphicle-bak`
     backups that setup made.
