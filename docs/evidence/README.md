# Evidence

The records behind the numbers in [benchmarks.md](../benchmarks.md). Each one
is published as it was written, apart from the changes listed for it. The
records name the internal pre-release builds they ran on; 0.1.0 is the first
public release.

## Engine parity: `engine-parity-0.9.88.json`

A summary of the comparison checks of the graph engine against the pinned
Graphify 0.9.88 reference, run on 13 October 2026 on a pre-release build
of 0.1.0 (13 October 2026; release profile):

- the binary, source set and build (profile, features, compiler) that were
  checked, and the version, commit and archive SHA-256 of the reference;
- each check, with its exit status, its time in seconds and the
  SHA-256 of its script in `crates/graphicle-graph/tools/`;
- each application module's receipt: the upstream source file and its SHA-256,
  the verdict, the number of native calls compared and the Rust files that
  implement it.

The checks' logs and the full receipts name local paths, so they stay with the
maintainers; a checkout recreates them. To run the checks yourself, see
`crates/graphicle-graph/README.md`, section "Verification".

`harness_sha256` is the SHA-256 of the harness-file table that the receipts
bind (every `tools/*.py` of the engine and the two upstream-review records),
as it stood in the maintainers' tree on 13 October. There `tools/verify_rust.py`
also runs checks that are not part of this repository; this repository's
copy leaves them out. On 13 October the same table computed from this
repository (`tools/qualification.py`'s `harness_sources()` in
`crates/graphicle-graph/`) gave
`174b5976ea56122b56c02f2d3003dd03eca20daf5907645769d11addee8e46f6`.

On 14 October the checks were widened (below), which changed nine check
scripts and two shared harness modules (`isolated_upstream.py` and
`upstream_naming.py`). The table computed from this repository now
gives `3ac6c1126c2c9969c30d5bc2a1799ca9bd69652806bb60c6c1ffbf2201753396`
(`741bff758a58ce487145d0101b761477055ae48a16f50e8ab13380790b3b6020` in the
maintainers' tree, which the 14 October run bound). The source set and every
other harness file are the same in both.

## Upstream test files: `upstream-tests-0.9.88.json`, `upstream-tests-known-differences.md` and `upstream-tests-not-run.md`

The checks run most of the test files of Graphify 0.9.88 against the
engine. A file counts as run when at least one of its tests runs and passes in
a check, or when the engine fails one of its tests and the check's receipt
records that as a known difference.

- [upstream-tests-0.9.88.json](upstream-tests-0.9.88.json) gives each file's
  status: run (with the checks that run it and its passing test count), not
  applicable, or not yet run, with the reasons.
- [upstream-tests-known-differences.md](upstream-tests-known-differences.md)
  lists the tests that the engine fails, each with the reason, and the files
  first run on 14 October, with the tests their checks leave out.
- [upstream-tests-not-run.md](upstream-tests-not-run.md) lists the other
  files, each with the reason it is not run: not applicable to the engine, or
  not yet run.

Before 14 October the checks ran fewer files. Summaries of that time counted
every file a receipt named, and some of those run no test: the receipt names
them only as not run, or every test the check selects from them is
deselected or skipped. They are now listed as not run.

How the 14 October run was made: the qualified release build (the default
features plus `postgres`, `bedrock` and `transcription`) of the same
pre-release engine source for Linux (x86_64), with Rust 1.95.0, binary SHA-256
`372dc90420a6683b8d90fa6123b3901fbf98a1acc621b07f08238d48d76729a0` and source
set `85f2d451090c1e62c2c25d49b780a771151c7d1e1ebf415907f93d68716bc1ea`, the
same source set as the 13 October run. `whisper-rs-sys` used its bundled
bindings, as the host has no libclang. The reference was the pinned archive
of Graphify 0.9.88 (SHA-256 `b0681491…`), with a Python 3.12.3 environment
from `tools/reference-requirements.txt` and
`tools/comparison-requirements.txt` (`graphifyy` 0.9.88 from PyPI).
`tools/qualify_all.py` ran every check on the widened harness and on the
harness of 13 October, with the same results: the checks pass, apart from
those that stop on that host, which refuses IPv4 loopback connections (`check_export_graphdb`,
`check_llm`, `check_native_workflows`, `check_pg_introspect`, `check_prs`)
and has no ffmpeg (`check_transcribe`); in `check_export` a graph layout
position differs there from NetworkX's by 2.2e-9, against a tolerance of
2e-9. A pytest plugin that only records outcomes was loaded into a second run
of every check to record each file's tests; no upstream test failed in it.
The widened checks have not yet run on macOS. The run's logs and receipts
name local paths, so they stay with the maintainers.

## Engine speed: `graphify-comparison-v4/`

The speed comparison of 8 October 2026 between Graphify 0.9.77 (Python) and a
pre-release build of the engine, which then followed Graphify 0.9.77, on one
Linux host: `PROTOCOL.json` (the method, the pinned inputs and the reference
environment) and `RESULTS.json` (every timed run, with its command, exit
status, time, peak memory, load average and the SHA-256 of the graph it wrote,
and the per-case medians).

Changed for publication: the host's working folders in paths are replaced by
`<run>`, `<inputs>`, `<build>` and `<reference>`. Nothing else differs. The
SHA-256 of the files as recorded:

| File | As recorded | As published |
|---|---|---|
| `PROTOCOL.json` | `361c8a15cdeaa7bf2e79517ac1bb1fc8ebc957821f57097b8994f1eb8f610f96` | `ddd5baa7d510942bb865c683b42aa8babbc30330dc57fcbf8636093e335f0ba1` |
| `RESULTS.json` | `639aed763705ba8ffe09151dcaef7f6bfba1dfd4eb0817a49e5501db12f43cb0` | `f7fd3bc08cce38c7979aa1bccf34d18fb45c7e43f40ff9e6d3ee5a2821b3a9f8` |

`RESULTS.json` names the recorded protocol by its original SHA-256. The
measurement driver is not published; `PROTOCOL.json` records its SHA-256 and
describes its method.

## Engine speed and peak memory against Graphify 0.9.100: `graphify-comparison-2026-11-01/`

The method record of the comparison of 1 November 2026 between Graphicle 0.1.11
and Graphify 0.9.100 (with Graphicle 0.1.0 where named), whose results are the
section [Graphicle 0.1.11 against Graphify 0.9.100](../benchmarks.md#graphicle-0111-against-graphify-09100)
of `benchmarks.md`:

- `PROTOCOL.md`: the protocol, registered at 2026-11-01T05:52:49Z, before the
  first measured run (05:53:29Z). It names the programs and how each was
  obtained, the machine and its CPU pinning, and each step's method and
  changes from the earlier published runs.
- `AMENDMENTS.md`: four notes added after registration, each before any result
  it could affect was read. None changes the method.
- `ranges/`: the registered commit ranges for updates after commits (pinned on
  15 October 2026 and reused unchanged).
- `graphify-constraints.txt`: the package constraints Graphify 0.9.100 was
  installed with.

Changed for publication, in `PROTOCOL.md` and `AMENDMENTS.md` only: the
machine's job and cluster identifiers, the host's working folders and the
names of private files are removed or replaced by a description (for example
`<run>` for the working folder, "an archive of the 0.1.11 release source" for
a private file name), and private names for the parallel run slots and
earlier protocols are replaced by plain descriptions. Nothing else differs.
`ranges/` and `graphify-constraints.txt` are published as recorded. The SHA-256
of the files:

| File | As registered | As published |
|---|---|---|
| `PROTOCOL.md` | `1be4dc64aacdcb5f29741d6af4ea0782763b7c36b2d44dfc929a5fcaf1fa07fc` | `48dd49e896c0143015fd35bfa77ebb58316096101b778c1a1b7bb113129c38d8` |
| `AMENDMENTS.md` | `6088c5fe462b1b9a98048605d1e5f3d5232127e1a7827cfa255ae721a7877ba0` | `0b39333413fccc12a489697d0a2521e7983c55d31dd9fe8f5a1130480e3d0335` |
| `graphify-constraints.txt` | `ba43ffcda305798241abcd1ac5adacdee6e9f23a7e363ae9a15a93cf012261cc` | the same |
| `ranges/prometheus_prometheus.json` | `4dfeea6477e02a4b80cc4c6c88c4387f446723cb81f7e6e58e2958b975bb4ce8` | the same |
| `ranges/python-poetry_poetry.json` | `e0c1d1f2a60bd2036517310327be2639bff45cd8acfede81dc7f29d3fc6a1b64` | the same |
| `ranges/tokio-rs_tokio.json` | `24d92403e4eb9ed3220dbe1fd7fc3da41cf566f14c1989e813152132be5c0749` | the same |

The measurement scripts that `PROTOCOL.md` names are not published; the
registration recorded their SHA-256 with the protocol's. The per-run rows,
logs and graph digests stay with the maintainers; the tables in
`benchmarks.md` are computed from them.
