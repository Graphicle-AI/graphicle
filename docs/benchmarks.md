# Benchmarks

This page gives the method and the status of each measurement. It names its
sources, and it reports results that did not favour Graphicle as well. Product
names are trademarks of their respective owners; no affiliation or
endorsement is implied.

## Summary

| Area | Status | Result |
|---|---|---|
| Code-graph engine: parity | Graphify 0.9.91: run on Linux, 15 October 2026; the full run on macOS is pending. Graphify 0.9.88: done, 13 October 2026; widened 14 October 2026 | Against Graphify 0.9.91, 89 of the 96 comparison checks pass on Linux; the other 7 stop on that host, as they did against 0.9.88 (IPv4 loopback, ffmpeg and one layout tolerance). Against Graphify 0.9.88, 96 of 96 pass, with reviewed upstream fixes applied to nine of the reference's modules; widened on 14 October, they run 354 of its 402 test files against the engine: 350 pass and 4 have known differences (7 tests). The limits are [below](#parity-with-graphify) |
| Code-graph engine: speed | Done, on a pre-release build that followed Graphify 0.9.77. The 0.1.0 engine: an exploratory survey, 14 October 2026; its registered quiet-host repeat is pending | Django (pre-release build): code-only extract 7.5 s against 72.8 s for Graphify 0.9.77. Kubernetes v1.37.1 (the 0.1.0 engine, which follows Graphify 0.9.88; exploratory, one run each on one Linux host, the same 16 CPUs, 8 workers, with four recorded deviations): code-only extract 198.4 s against 1,597.9 s for Graphify 0.9.88, the same graph, with about 2.1 times more peak memory for Graphicle. [Details](#speed-on-kubernetes) |
| Agent memory: comparison with seven memory systems | Done, on development sets, with a proxy evaluator | Every system's scores and the 21 registered comparisons are [below](#agent-memory-comparison-with-seven-memory-systems). None of them is a headline claim |
| Agent memory: retrieval defaults | Chosen | Model-free retrieval runs on development data, plus held-out checks; their records are not published yet, so no numbers are given |

## Code-graph engine

### Parity with Graphify

The engine is a Rust port of Graphify. It follows Graphify
0.9.91 (tag commit `10130e79`): the port was made from 0.9.77 (`5c7b8479`),
the eleven releases 0.9.78 to 0.9.88 were ported for 0.1.0, and the three
releases 0.9.89 to 0.9.91 for 0.1.1. Against Graphify 0.9.88 it passed the
96 comparison checks, and all 95 application modules had passing receipts.
Against Graphify 0.9.91 the checks have run on Linux, where 7 of them stop
for reasons of that host (below); the full run on macOS, which records every
module's receipt, is still to be made for 0.9.91. That is the parity claim,
with these limits:

- **Graphify 0.9.91 (15 October 2026).** The qualified release build of the
  0.1.1 source (binary SHA-256 `adf81bb2…`) ran against the pinned archive of
  Graphify 0.9.91 (`10130e79`, archive SHA-256 `f4916b0c…`, with `graphifyy`
  0.9.91 from PyPI) on Linux (x86_64). Of the 96 checks, 89 pass:
  `tools/qualify_all.py` passed 88, and `check_detect` passed when run on its
  own (in the full run, one upstream test could not bind a Unix socket
  because the run's temporary path was longer than the system allows). The
  other 7 stop on that host, as they did against 0.9.88: 5 need IPv4
  loopback connections, which the host refuses (4 of them pass with their
  test servers on the host's own address; in `check_native_workflows` the
  reference's own client then fails to set up TLS on that host),
  `check_transcribe` needs ffmpeg, which the host lacks, and in
  `check_export` a graph layout position differs from NetworkX's by 2.2e-9,
  against a tolerance of 2e-9. Of the 19 test files that 0.9.89 to 0.9.91
  add and the 25 they change, the checks run all but five: two test
  Graphify's skill text and skill generator, one patches the reference's
  own subprocess call (the engine's equivalent has a Rust test), and the two
  Ollama files drive mocked Python clients, so `check_llm` compares the
  engine's Ollama requests with the reference's instead. On
  Kubernetes, prometheus and tokio, the engine's `update --force
  --no-cluster` gives the same nodes and edges as Graphify 0.9.91's on the
  same folder, record for record and in the same order; the files differ
  only in where some edges place their `context` key, as 0.1.0's did. The
  items below describe the 0.9.88 runs unless they say otherwise.
- **What the checks do.** Each check runs upstream tests and comparisons
  against the Rust engine and compares the engine's results with the pinned
  Python release. Together they run 354 of Graphify 0.9.88's 402 test files
  (`tests/test_*.py`) and cover all 95 application modules (84,134 source
  lines). A file counts as run when at least one of its tests runs and passes
  in a check, or when the engine fails one of its tests (a known difference,
  below).
- **Widened on 14 October 2026.** Before that day the checks ran 279 of the
  test files; 75 more were added, mostly extraction and cross-file
  resolution across languages, plus files for the command line, graph
  building, file discovery, exports and other modules. They are listed in
  [evidence/upstream-tests-known-differences.md](evidence/upstream-tests-known-differences.md).
  The widened checks ran on Linux (x86_64) with the same engine source. There
  89 of the 96 checks pass, and the other 7 stop on that host, before the
  change and after it alike: 5 need IPv4 loopback connections, which the
  host refuses, one needs ffmpeg, which it lacks, and in one a graph layout
  position differs there from NetworkX's by 2.2e-9, against a tolerance of
  2e-9. Every upstream test that ran there passed, apart from the known
  differences. The widened checks have not yet run on macOS.
- **Known differences in upstream tests.** 7 tests in 4 files fail against
  the engine. Each is recorded, with the reason, in
  [evidence/upstream-tests-known-differences.md](evidence/upstream-tests-known-differences.md)
  and deselected by its exact name. Six are command-line messages and help
  text that the engine words differently or does not print; one is a second
  `case_of` edge for TypeScript enum members whose names differ only in case.
- **Tests that are not run.** The other 48 test files are listed in
  [evidence/upstream-tests-not-run.md](evidence/upstream-tests-not-run.md),
  each with its reason: 20 test something the engine does not contain or
  that exists only in Python (Python packaging and interpreter details,
  private Python functions, Graphify Cloud's call to action, Graphify's own
  skill text and skill generator, and its test fixtures), and 28 test
  behaviour the engine ports but are not run yet, mostly in the
  language-model module (11 files) and the installer and agent integration
  (9).
- **A reviewed reference.** The checks compare with the pinned release plus
  reviewed upstream fixes, applied in memory to nine of its modules (cache,
  extract, cluster, markdown, sql, vbnet, apex, commonlisp and sln); the
  engine has the same fixes.
  `crates/graphicle-graph/migration/upstream-review/REVIEW.md` lists them.
  Where a fix applies, the engine's results differ from released Graphify
  0.9.88. For example, community re-splits follow a non-default
  `--resolution`, so barbell(15,4) at resolution 0.1 gives 2 communities
  where Graphify 0.9.88 gives 3.
- **Known differences.** PDF text comes from `pdf-extract`, not pypdf, so PDF
  word counts and text can differ, and command-line progress messages are the
  engine's own.
- **Known difference.** On gohugoio/hugo v0.167.0, the graph after
  `graphicle graph update` differs from Graphify 0.9.88's in one attribute of
  one node: the stored front matter of a Markdown page that closes a YAML list
  at the start of a line. Graphicle's YAML parser rejects that list, so
  Graphicle stores the page's front matter fields as text and loses the list's
  nine entries. Nodes, edges and communities are the same, code-only `extract`
  gives the same graph, and answers are not affected.
- **Names.** Folder, environment-variable, command and installed-file names
  are translated by one documented table
  (`crates/graphicle-graph/tools/naming.py`), which the checks apply.
  Graphify's `graphify-out/` folder, for example, is Graphicle's
  `.graphicle/`.
- **The checked build.** All 96 checks last passed on 13 October 2026, on a
  pre-release build (commit cce5fd4, 13 October 2026) of the engine
  (`graphicle-graph`) for macOS, release profile; the widened checks ran on
  14 October 2026 on a Linux release build of the same source. Both builds
  use the default features plus `postgres`, `bedrock` and `transcription`,
  which some checks exercise. The engine pins tree-sitter 0.25.10, the parser
  runtime that Python Graphify 0.9.88 and 0.9.91 use, so files that a grammar rejects
  recover the same way. Release archives contain the default build, without
  those three features.
- **The record.** [evidence/engine-parity-0.9.88.json](evidence/engine-parity-0.9.88.json)
  summarizes the 13 October run: each check with its result, and each
  module's receipt. [evidence/upstream-tests-0.9.88.json](evidence/upstream-tests-0.9.88.json)
  gives the status of each of the 402 upstream test files.
  `crates/graphicle-graph/migration/STATUS.md` lists every ported module, and
  `crates/graphicle-graph/migration/upstream-review/REVIEW.md` records the
  upstream fixes that were reviewed and the known limitations.

The opt-in additions (`--precise`, `extract --docs`, `extract --routes`,
`retrieve`, `serve --watch`) go beyond Graphify 0.9.91, so the parity claim
does not cover them. Each one is off unless you ask for it.

### Speed against Graphify 0.9.77

**Version.** This record was measured on 8 October 2026 with a pre-release
build of the engine, when the engine followed Graphify 0.9.77, and it compares
that build with Graphify 0.9.77. The 0.1.0 engine follows Graphify 0.9.88. Its
first registered timing run against Graphify 0.9.88 (13 and 14 October 2026)
shared its host with other work, so, as that run's protocol requires, it will
be repeated on a quiet host and both runs will be published here. The 0.1.0
engine's first published timing is the exploratory survey in
[Speed on Kubernetes](#speed-on-kubernetes) below; the registered quiet-host
repeat is still pending. The record is kept as it was published.

| Repository | Operation | Graphify 0.9.77 (Python) | Graphicle (pre-release build of 8 October 2026) | Speed-up | Peak memory, Python / Graphicle |
|---|---|---|---|---|---|
| Django | Code-only extract | 72.8 s | 7.5 s | 9.7× | 1,495 / 1,005 MiB |
| Django | Update after one added definition | 107.9 s | 9.8 s | 11.0× | 4,443 / 1,270 MiB |
| Django | Update with no change | 77.3 s | 5.9 s | 13.2× | 4,444 / 1,267 MiB |
| defu (113 nodes) | Code-only extract | 1.47 s | 0.08 s | 17.6× | 107 / 71 MiB |
| defu | Update after one added definition | 1.18 s | 0.10 s | 11.7× | 57 / 73 MiB |
| scule (87 nodes) | Code-only extract | 1.44 s | 0.08 s | 18.6× | 107 / 67 MiB |
| scule | Update after one added definition | 1.13 s | 0.09 s | 12.4× | 56 / 73 MiB |

- **Method.** The two programs ran in alternating order, five times each per
  repository, on one Linux host with 8 extraction workers and no GPU or model.
  Each run used the same source path and a fresh output folder. The times are
  medians and include program start-up.
- **Memory.** Peak memory is the process tree's resident set, sampled every
  20 ms. On the two small repositories, Graphicle's updates use more memory
  than Python's.
- **Same output.** Both programs produced the same graph in every case: every
  node and edge attribute matched. Django's graph had 53,363 nodes and 119,744
  edges after the extract, and 53,847 nodes and 119,767 edges after the
  updates.
- **Caveats.** The host was shared, and a single added definition does not
  represent every kind of change.
- **Source.** Our measurement record "graphify-comparison-v4" of
  8 October 2026, published in
  [evidence/graphify-comparison-v4/](evidence/README.md#engine-speed-graphify-comparison-v4)
  with every timed run (protocol SHA-256 `361c8a15…8f610f96` as recorded,
  binary SHA-256 `0a1f83d1…60104de9`).

### Speed on Kubernetes

The first published timing of the 0.1.0 engine against Graphify 0.9.88 is an
exploratory survey, measured on 14 October 2026. **Label for every figure in
this section:** exploratory survey; one Linux host (Intel Xeon Platinum 8480C;
no GPU or model); for each repository, both programs pinned to the same 16
CPUs with 8 workers each; times include program start-up. Each figure also
names its repository and tag. No figure here is a registered claim, and no
figure is pooled across repositories. Kubernetes and VS Code had one run per
program, and four deviations from the registered plan apply to them (listed
under Method, below).

**Kubernetes v1.37.1** (Go, 31,256 files), code-only extract into an empty
output folder, one run per program:

| Program | Time | CPU time | Peak memory (PSS) | Graph written |
|---|---|---|---|---|
| Graphicle 0.1.0 (a Linux test build; deviation 2) | 198.4 s (3 min 18 s) | 262.5 s | 5,871 MiB (about 5.7 GiB) | 256,329 nodes, 1,400,392 edges |
| Graphify 0.9.88 | 1,597.9 s (26 min 38 s) | 1,729.1 s | 2,770 MiB (about 2.7 GiB) | the same 256,329 nodes and 1,400,392 edges |

- **Same graph, 8.1× faster on this run.** Both programs wrote identical node
  and edge records: the canonical digests of all node records and of all edge
  records are equal. Graphicle also used 6.6 times less CPU time.
- **Memory: Graphicle used about 2.1 times more.** Its peak PSS (the process
  tree's proportional set size, sampled every 0.5 s) was 5,871 MiB against
  2,770 MiB for Graphify, and the resident-set sum agrees (5,916 against
  2,797 MiB).
- **Updates stopped at the graph-file cap.** Kubernetes' `graph.json` is about
  570 MB, above the default 512 MiB cap on the graph file that both programs
  share. Every update of both programs (a priming update, then an update with
  nothing changed) stopped at that cap and exited with an error: Graphicle
  after about 3 s, when it read the stored graph and before re-extracting
  anything; Graphify after about 24 minutes (1,434 s and 1,430 s), when it read
  the stored graph to merge its new extraction. These are failed runs, so
  there is no update comparison. To update a graph this large with Graphicle,
  raise the cap with `GRAPHICLE_MAX_GRAPH_BYTES`, in bytes or as `<N>GB`
  (gibibytes): for example, `GRAPHICLE_MAX_GRAPH_BYTES=1GB graphicle graph update .`

**VS Code 1.141.1** (TypeScript, 19,971 files): not admitted, because the two
programs' graphs differ, so no speed-up is given. After the extract,
Graphicle's graph has 3 more nodes and 11 fewer edges than Graphify's (249,081
nodes and 974,695 edges against 249,078 and 974,706). The graphs after the
updates differ too: after the update with no change, nearly every node record
differs (249,266 of Graphicle's 253,812 against 249,263 of Graphify's
253,809), as do 585 against 596 edge records. The differences are reported as
found, not investigated. The times as measured, one run per
program, are not a comparison of the same output:

| Operation | Graphicle 0.1.0 | Graphify 0.9.88 |
|---|---|---|
| Code-only extract | 90.6 s | 393.2 s |
| Priming update | 132.0 s | 642.6 s |
| Update with no change | 110.7 s | 387.9 s |

**The survey's smaller repositories**, measured before access to the host was
lost: one warm-up, then 3 measured runs per program in alternating order.
Times are medians; a speed-up is the median of the per-run ratios, with the
least and most favourable run in brackets.

| Repository | Tag | Language | Files | Code-only extract: Graphicle / Graphify | Speed-up | Update with no change: Graphicle / Graphify | Speed-up | Peak memory (PSS), extract: Graphicle / Graphify | Same graph |
|---|---|---|---|---|---|---|---|---|---|
| pallets/flask | 3.1.3 | Python | 235 | 0.27 s / 1.98 s | 7.4× [7.2–7.8] | 0.16 s / 1.52 s | 9.3× [9.2–9.5] | not measured: runs too short | yes, in every run of every operation |
| BurntSushi/ripgrep | 15.2.0 | Rust | 221 | 0.36 s / 1.93 s | 5.4× [5.4–5.5] | 0.27 s / 1.59 s | 5.8× [5.8–5.9] | not measured: runs too short | yes, in every run of every operation |
| redis/redis | 8.10.2 | C | 1,861 | 2.49 s / 11.8 s | not admitted: graphs differ | 2.09 s / 10.1 s | not admitted: graphs differ | 373 / 413 MiB, not compared: graphs differ | no: every operation differs |
| prometheus/prometheus | v3.15.0 | Go | 1,686 | 3.44 s / 18.6 s | 5.4× [5.3–5.5] | 3.24 s / 18.8 s | 5.8× [5.7–5.8] | 487 / 455 MiB, no ratio: PSS and the resident-set sum disagree | yes, in every run of every operation |
| tokio-rs/tokio | tokio-1.53.3 | Rust | 867 | 1.03 s / 5.80 s | 5.6× [5.5–5.7] | 1.00 s / 5.46 s | 5.4× [5.4–5.7] | 205 / 230 MiB: 1.2 times less for Graphicle (median of the per-run ratios; the resident-set sum agrees) | yes, in every run of every operation |

- **redis.** After the extract, Graphify's graph has one more node and two
  more edges: `contains` edges from two Visual Studio solution files in
  `deps/jemalloc/msvc/`. After the update with no change, 15,945 node records
  differ in content (in the examples kept, the community name). Each program
  wrote the same graph in all of its own runs. Reported as found, not
  investigated.
- **Not run.** facebook/react was cut short in its warm-up when access to the
  host was lost, so it has no measured run; vercel/next.js, pandas-dev/pandas,
  scikit-learn/scikit-learn, spring-projects/spring-boot and rails/rails were
  not run. Kubernetes and VS Code ran later (deviation 3).

**Method.**

- **Registered first.** The survey was registered on 14 October 2026 at
  09:18 UTC, before any clone or measurement, as a dated amendment to the
  protocol of the registered timing run named above. It lists 13 repositories
  of several languages and sizes, tests none of that protocol's hypotheses,
  and publishes every result whatever it shows.
- **Exploratory.** Kubernetes and VS Code had one run per program, with no
  warm-up and a 45-minute limit per run; the smaller repositories had one
  warm-up and 3 measured runs. With so few runs, the figures describe these
  runs only.
- **Host.** One Linux host with Intel Xeon Platinum 8480C processors; no GPU
  or model was used. For each repository, both programs were pinned to the
  same 16 CPUs with 8 extraction workers each and ran one after the other.
  The smaller repositories ran on CPUs 32-47 while another registered timing
  run used the host's other socket. Before each program run, the harness
  waited until its own CPUs were under 5% busy.
- **Operations.** `extract <repo> --code-only --force` into an empty output
  folder, then a priming update and an update with nothing changed; the page
  cache was not dropped. Each repository was pinned to its latest stable
  release tag when it was cloned.
- **Measurements.** Wall time, including program start-up; CPU time of the
  process tree; peak memory as the PSS summed over the process tree, sampled
  every 0.5 s. A memory ratio is given only when the resident-set sum agrees
  in direction.
- **Same output.** A speed-up is given only when both programs wrote the same
  graph, with every node and every edge record equal, in every measured run
  of that operation. Differences are reported as found.
- **Programs.** Graphify 0.9.88 (Python 3.12.13) in a hash-locked environment.
  Graphicle: `graphicle-graph` 0.1.0 built for Linux on the measuring host from
  the release source, not the published download; for the smaller
  repositories SHA-256 `32d7f800…`, and for Kubernetes and VS Code see
  deviation 2.
- **Deviations from the registration** (Kubernetes and VS Code):
  1. Kubernetes and VS Code ran at the same time, one on each of the host's
     two processor sockets (CPUs 32-47 and 64-79), where the registration ran
     one repository at a time. Other work ran on Kubernetes' socket (CPUs
     0-15) during all of Graphicle's Kubernetes extract, and on the other
     socket during Graphify's failed Kubernetes updates. Its effect was not
     measured.
  2. The Graphicle binary is a Linux test build of 0.1.0 (SHA-256
     `491a4b35…`), made on the measuring host from the release source; its
     engine source is identical to the release's. It is not the published
     download.
  3. The runs started after the survey's registered time box, and out of its
     registered order.
  4. No host-wide load record was kept; the registration asked for one every
     3 minutes.
- **Records.** [evidence/engine-speed-2026-10-14.json](evidence/engine-speed-2026-10-14.json)
  ([described here](evidence/README.md#engine-speed-survey-engine-speed-2026-10-14json))
  holds every program run of Kubernetes and VS Code and the smaller
  repositories' medians, with the digests of the graphs written. The full run
  records (per-run rows, analysis output, setup receipts and logs) name local
  paths, so they stay with the maintainers.

## Agent memory: comparison with seven memory systems

Seven memory systems and a build of Graphicle answered the same questions
from 11 to 13 October 2026, on GPU nodes of one type with the same LLM server
setup and evaluation harness, under a protocol that was registered before
any answer was scored. The other systems ran on 11 and 12 October. Graphicle's
arm configuration was registered on 13 October, after their scores were
final and before its own first model call ([Method](#method)). Its records
call the comparison R6.

### Labels

These labels apply to every result in this section:

- **Graphicle R5 with the evaluation renderer render_v2.py (not shipped).**
  This is the Graphicle arm: a pre-release build of Graphicle Core
  (commit ef4e0fe, 13 October 2026; binary SHA-256 `2447d547…`).
  - It was built with the `local-inference` feature, which the release
    archives leave out ([install.md](install.md#build-from-source)), and with
    the change that lets `worker_concurrency` go up to 64 (it was 4), which
    0.1.0 ships. The arm set it to 16, so up to 16 observer jobs ran at once.
  - It embedded with an int8 ONNX export of bge-small-en-v1.5
    (`Xenova/bge-small-en-v1.5`) in its own process, on the CPU, not through
    the shared embedding server that the other systems called.
  - The reader's context was composed from its `context` and
    `observer.context` results by `render_v2.py`, a renderer of the
    evaluation harness that this release does not ship.

  A 0.1.0 release archive therefore cannot run this configuration as it ran.
- **Development sets.** LoCoMo-10, LME-84 and BEAM-100K are development sets:
  Graphicle's context composition was selected on them with this reader and
  judge. R6 has no held-out set.
- **LME-84** is category-balanced (84 LongMemEval-S questions, 12 from each of
  7 strata); it is not a LongMemEval score.
- **BEAM-100K** is judge validated on LongMemEval only.
- **Proxy evaluator.** Qwen3.8-27B-FP8 is the reader, the judge and every
  system's LLM. These numbers cannot be set against any vendor's published
  numbers.
- **Readers.** Every row gives the strong reader (secondary) beside the weak
  reader (primary), and LLM calls and tokens beside accuracy.

### Scores

Each score is in points (0–100) on the same questions, with a 95% interval:
accuracy on LoCoMo-10 and LME-84, and the mean rubric score on BEAM-100K.
Every question stays in the denominator, and a failure scores 0.
The second prompt path is the Agent Memory Benchmark's own prompt, read by the
weak reader. Each system ran in its best declared configuration. LLM calls
and tokens are each system's own model calls during ingestion, per
conversation ([how they were counted](#calls-and-tokens)).

**LoCoMo-10** (development set; 1,540 questions in 10 conversations)

| System (configuration) | Weak reader, primary [95% CI] | Second prompt path [95% CI] | Strong reader, secondary [95% CI] | LLM calls per conversation | Prompt / completion tokens per conversation |
|---|---|---|---|---|---|
| Graphicle R5 with the evaluation renderer render_v2.py (not shipped) | 87.66 [85.13, 90.20] | 87.99 [85.95, 90.01] | 87.66 [86.39, 88.86] | 12.2 | 43.4K / 13.5K |
| Hindsight 0.10.2, observations on (`hindsight-obs`) | 87.27 [85.63, 89.06] | 87.53 [85.90, 89.27] | 87.14 [86.04, 88.33] | 160.0 | 1.14M / 69.4K |
| Mem0 OSS 2.2.1, dated (`mem0`) | 81.88 [79.59, 84.36] | 80.97 [79.02, 83.14] | 82.01 [80.00, 83.91] | 332.0 | 3.18M / 34.3K |
| MemMachine 0.4.0 | 79.55 [77.26, 81.97] | 80.26 [77.60, 83.01] | 81.30 [79.22, 83.76] | 22.6 | 140.3K / 3.7K |
| Honcho 3.2.2 | 79.16 [76.21, 82.15] | 77.73 [74.57, 80.45] | 80.45 [78.21, 83.00] | 221.3 | 1.25M / 78.2K |
| Cognee 1.6.3 | 69.48 [67.32, 71.24] | 68.57 [65.75, 71.18] | 70.26 [67.18, 72.68] | 205.9 | 290.7K / 65.1K |
| Graphiti (graphiti-core 0.30.2) | 46.49 [43.99, 48.88] | 37.14 [34.28, 40.07] | 49.29 [46.51, 52.05] | 1,362.6 | 4.52M / 108.7K |
| Mastra Observational Memory (@mastra/memory 1.36.0) | 38.57 [33.56, 43.10] | 37.21 [32.46, 41.80] | 43.51 [38.58, 48.14] | 22.3, and 1 per question | 356.4K / 15.1K |
| Calibration: Hindsight's published contexts, read by the same reader and judge | 88.18 [86.31, 90.29] | — | 87.66 [86.39, 89.07] | — | — |

**LME-84** (development set; category-balanced; not a LongMemEval score; 84 questions)

| System (configuration) | Weak reader, primary [95% CI] | Second prompt path [95% CI] | Strong reader, secondary [95% CI] | LLM calls per conversation | Prompt / completion tokens per conversation |
|---|---|---|---|---|---|
| Graphicle R5 with the evaluation renderer render_v2.py (not shipped) | 92.86 [86.90, 97.62] | 94.05 [89.29, 98.81] | 95.24 [90.48, 98.81] | 45.8 | 161.8K / 63.4K |
| MemMachine 0.4.0 | 92.86 [86.90, 97.62] | 92.86 [86.90, 97.62] | 92.86 [86.90, 97.62] | 47.9 | 152.3K / 8.7K |
| Hindsight 0.10.2 (`hindsight`) | 85.71 [77.38, 92.86] | 86.90 [79.76, 94.05] | 90.48 [83.33, 96.43] | 258.8 | 1.00M / 55.0K |
| Mem0 OSS 2.2.1, dated (`mem0`) | 85.71 [78.57, 92.86] | 83.33 [75.00, 90.48] | 89.29 [82.14, 95.24] | 247.5 | 2.43M / 36.6K |
| Honcho 3.2.2 | 76.19 [66.67, 84.52] | 83.33 [75.00, 90.48] | 83.33 [75.00, 90.48] | 220.5 | 1.47M / 75.9K |
| Cognee 1.6.3 | 75.00 [65.48, 83.33]; as read: 82.14 [73.81, 90.48] | 72.62 [63.10, 82.14]; as read: 80.95 | 78.57 [69.05, 86.90]; as read: 86.90 | 573.0 | 819.3K / 312.4K |
| Mastra Observational Memory (@mastra/memory 1.36.0) | 60.71 [50.00, 71.43] | 66.67 [57.14, 76.19] | 64.29 [53.57, 73.81] | 55.8, and 1 per question | 1.01M / 42.8K |
| Graphiti (graphiti-core 0.30.2) | not run (cost; decided on 11 October 2026) | — | — | — | — |
| Calibration: Hindsight's published contexts, read by the same reader and judge | 88.10 [80.95, 94.05] | — | 91.67 [85.71, 97.62] | — | — |

**BEAM-100K** (development set; judge validated on LongMemEval only; 400
questions in 20 conversations; the score is the mean rubric score)

| System (configuration) | Weak reader, primary [95% CI] | Second prompt path [95% CI] | Strong reader, secondary [95% CI] | LLM calls per conversation | Prompt / completion tokens per conversation |
|---|---|---|---|---|---|
| Graphicle R5 with the evaluation renderer render_v2.py (not shipped) | 67.32 [64.71, 69.96] | 62.70 [59.81, 65.47] | 69.82 [67.28, 72.34] | 24.3 | 126.4K / 33.3K |
| Mem0 OSS 2.2.1, dated (`mem0`) | 63.14 [59.77, 66.63] | 57.64 [54.37, 60.97] | 64.79 [60.86, 68.72] | 232.7 | 2.25M / 27.4K |
| Hindsight 0.10.2, observations on (`hindsight-obs`) | 62.77 [59.58, 65.71] | 59.77 [56.13, 63.48] | 68.67 [65.00, 72.01] | 305.4 | 3.20M / 139.8K |
| MemMachine 0.4.0 | 61.90 [58.36, 65.22] | 57.81 [54.06, 61.27] | 66.13 [62.48, 69.43] | 10.2 | 66.4K / 1.7K |
| Cognee 1.6.3 | 58.46 [55.68, 61.26] | 60.03 [56.34, 63.52] | 64.49 [61.28, 67.55] | 593.3 | 904.9K / 297.7K |
| Honcho 3.2.2 | 57.62 [54.79, 60.30] | 54.44 [51.03, 57.85] | 63.45 [60.58, 66.21] | 146.1 | 959.2K / 49.4K |
| Mastra Observational Memory (@mastra/memory 1.36.0) | 52.51 [49.55, 55.51] | 46.14 [42.36, 49.65] | 57.22 [53.19, 61.08] | 18.4, and 1 per question | 354.6K / 14.2K |
| Graphiti (graphiti-core 0.30.2) | 39.43 [35.65, 43.42] | 36.95 [33.97, 39.94] | 41.91 [38.52, 45.59] | 1,183.6 | 3.91M / 96.1K |
| Calibration: Hindsight's published contexts, read by the same reader and judge | 56.26 [51.65, 60.01] | — | 60.61 [54.63, 65.46] | — | — |

- **Failures** (each scores 0) of Graphicle R5 with the evaluation renderer
  render_v2.py (not shipped): one weak answer on LoCoMo-10 was cut at 4,096
  tokens; on the second prompt path, 17 answers on LoCoMo-10 and 14 on
  BEAM-100K were truncated or invalid JSON, and one BEAM-100K request failed
  at the evaluation's metering proxy (HTTP 502); the strong reader gave no
  usable final answer for 12 of 1,540 LoCoMo-10 questions and 8 of 400
  BEAM-100K questions.
- **Cognee on LME-84.** Three of Cognee's model requests timed out while it
  re-ingested failed conversations, so the registered failure rule scores 8 of
  its questions 0 on every path. "As read" scores them as they were answered.
- **The calibration row** replays the contexts of Hindsight's published Agent
  Memory Benchmark run through the same reader and judge. It checks whether
  this setup handicaps Hindsight; it is not a vendor-published number.

### The 21 registered comparisons

Each comparison is Graphicle R5 with the evaluation renderer render_v2.py (not
shipped) against one system's best declared configuration on one dataset, on
the weak reader. All three datasets are development sets.

| # | Dataset | System (configuration) | Weak scores: Graphicle R5 with the evaluation renderer render_v2.py (not shipped) / system | Difference [95% CI] | Bonferroni interval (21 comparisons) | Holm-adjusted p | Second prompt path: difference [95% CI] | Strong reader: difference [95% CI] | Verdict |
|---|---|---|---|---|---|---|---|---|---|
| 1 | LoCoMo-10 | Hindsight (`hindsight-obs`) | 87.66 / 87.27 | +0.39 [−0.80, +1.57] | [−1.41, +2.21] | 1.000 | +0.45 [−0.75, +1.81] | +0.52 [+0.00, +1.01] | matches |
| 1 | LoCoMo-10 | Hindsight's published contexts | 87.66 / 88.18 | −0.52 [−2.68, +1.78] | [−3.68, +3.05] | (same comparison) | no second path | +0.00 [−0.67, +0.63] | (same comparison) |
| 2 | LoCoMo-10 | Mem0 (`mem0`) | 87.66 / 81.88 | +5.78 [+3.99, +7.23] | [+2.74, +7.78] | 0.027 | +7.01 [+5.40, +8.74] | +5.65 [+4.31, +7.05] | beats |
| 3 | LoCoMo-10 | Graphiti | 87.66 / 46.49 | +41.17 [+37.68, +44.76] | [+35.69, +46.81] | 0.025 | +50.84 [+47.49, +54.38] | +38.38 [+35.18, +41.62] | beats |
| 4 | LoCoMo-10 | MemMachine | 87.66 / 79.55 | +8.12 [+5.95, +10.24] | [+4.88, +11.43] | 0.025 | +7.73 [+5.93, +9.11] | +6.36 [+4.13, +8.27] | beats |
| 5 | LoCoMo-10 | Mastra OM | 87.66 / 38.57 | +49.09 [+43.93, +54.58] | [+40.67, +58.23] | 0.025 | +50.78 [+45.87, +55.74] | +44.16 [+39.53, +49.15] | beats |
| 6 | LoCoMo-10 | Cognee | 87.66 / 69.48 | +18.18 [+15.43, +21.18] | [+14.23, +22.87] | 0.025 | +19.42 [+16.34, +22.89] | +17.40 [+14.70, +20.37] | beats |
| 7 | LoCoMo-10 | Honcho | 87.66 / 79.16 | +8.51 [+6.90, +9.96] | [+5.97, +10.64] | 0.025 | +10.26 [+8.47, +12.40] | +7.21 [+5.55, +8.56] | beats |
| 8 | LME-84 | Hindsight (`hindsight`) | 92.86 / 85.71 | +7.14 [+0.00, +14.29] | [−3.57, +19.05] | 0.287 | +7.14 [+0.00, +14.29] | +4.76 [−2.38, +11.90] | matches |
| 9 | LME-84 | Mem0 (`mem0`) | 92.86 / 85.71 | +7.14 [+0.00, +14.29] | [−3.57, +19.05] | 0.287 | +10.71 [+2.38, +19.05] | +5.95 [−1.19, +14.29] | matches |
| 10 | LME-84 | Graphiti | not run | — | — | 1 (entered) | — | — | not run |
| 11 | LME-84 | MemMachine | 92.86 / 92.86 | +0.00 [−5.95, +7.14] | [−10.71, +10.71] | 1.000 | +1.19 [−2.38, +4.76] | +2.38 [−3.57, +8.33] | matches |
| 12 | LME-84 | Mastra OM | 92.86 / 60.71 | +32.14 [+21.43, +42.86] | [+15.48, +48.81] | 0.002 | +27.38 [+17.86, +36.90] | +30.95 [+20.24, +41.67] | beats |
| 13 | LME-84 | Cognee | 92.86 / 75.00 | +17.86 [+8.33, +27.38] | [+2.38, +33.33] | 0.006 | +21.43 [+11.90, +30.95] | +16.67 [+7.14, +26.19] | no registered word: fails the as-read condition |
| 13 | LME-84 | Cognee, as read | 92.86 / 82.14 | +10.71 [+1.19, +20.24] | — | 0.181 | +13.10 [+4.76, +21.43] | +8.33 [+0.00, +16.67] | (the condition of comparison 13) |
| 14 | LME-84 | Honcho | 92.86 / 76.19 | +16.67 [+7.14, +26.19] | [+2.38, +32.14] | 0.009 | +10.71 [+2.38, +19.05] | +11.90 [+4.76, +20.24] | beats |
| 15 | BEAM-100K | Hindsight (`hindsight-obs`) | 67.32 / 62.77 | +4.55 [+1.63, +7.91] | [+0.21, +9.63] | 0.014 | +2.93 [−0.40, +6.36] | +1.14 [−2.51, +4.99] | no registered word: fails the second-path condition |
| 16 | BEAM-100K | Mem0 (`mem0`) | 67.32 / 63.14 | +4.17 [+1.07, +7.27] | [−0.45, +8.88] | 0.038 | +5.06 [+1.72, +8.24] | +5.03 [+1.59, +8.55] | beats |
| 17 | BEAM-100K | Graphiti | 67.32 / 39.43 | +27.88 [+23.39, +32.17] | [+21.02, +34.39] | 0.002 | +25.75 [+21.72, +29.64] | +27.90 [+23.14, +32.51] | beats |
| 18 | BEAM-100K | MemMachine | 67.32 / 61.90 | +5.42 [+2.02, +8.75] | [−0.00, +10.60] | 0.025 | +4.89 [+1.44, +8.42] | +3.68 [+0.04, +7.49] | beats |
| 19 | BEAM-100K | Mastra OM | 67.32 / 52.51 | +14.80 [+10.73, +18.71] | [+8.57, +20.81] | 0.002 | +16.56 [+12.40, +20.54] | +12.60 [+7.89, +17.56] | beats |
| 20 | BEAM-100K | Cognee | 67.32 / 58.46 | +8.86 [+6.26, +11.42] | [+4.93, +12.67] | 0.002 | +2.66 [−1.10, +6.45] | +5.33 [+2.49, +8.16] | no registered word: fails the second-path condition |
| 21 | BEAM-100K | Honcho | 67.32 / 57.62 | +9.70 [+7.49, +11.81] | [+6.42, +12.94] | 0.002 | +8.26 [+4.74, +11.51] | +6.37 [+2.93, +9.99] | beats |

How to read it:

- **Difference** is Graphicle R5 with the evaluation renderer render_v2.py
  (not shipped) minus the system, in points, on the same questions.
- **The rule** was registered before the scores existed. "Beats" needs a
  Holm-adjusted p below 0.05 over the 21 comparisons, a weak-reader interval
  above zero and a second-path interval above zero. "Matches" means that the
  weak-reader interval spans zero, and "trails" that it lies below zero. A
  "beats" whose strong-reader interval lies below zero would become
  "conflicting readers"; none did. Neither the strong reader nor the second
  path can change a tie into a "beats".
- **No registered word.** Comparisons 13, 15 and 20 have a weak-reader
  interval above zero but fail another condition of "beats", and the rule has
  no word for that case.
- **Hindsight on LoCoMo-10.** A "beats" had to hold against both its best
  fresh configuration and its published contexts, each under the full rule.
  The comparison's p is the larger of the two.
- **Cognee on LME-84.** A "beats" had to hold also with its 8 zeroed
  questions scored as read, a condition registered before Graphicle's arm ran.
- **Graphiti on LME-84** was not run because of its cost, and it is counted
  in the Holm correction with p = 1.

### What the results allow us to say

The sentences below are the claims the registered analysis allows. Each one
carries the labels above: development sets, the proxy evaluator, and
Graphicle R5 with the evaluation renderer render_v2.py (not shipped), which
"it" means here. None of them is a headline claim: a "beats" needs held-out
confirmation first, and R6 has no held-out set.

1. On LoCoMo-10 (development set; Graphicle's context composition was selected
   on it with this reader and judge), Graphicle R5 with the evaluation renderer
   render_v2.py (not shipped) beats Mem0 (+5.78 points [+3.99, +7.23]),
   MemMachine (+8.12), Honcho (+8.51), Cognee (+18.18), Graphiti (+41.17) and
   Mastra OM (+49.09) under the pre-registered rule (Holm-adjusted p ≤ 0.027
   over 21 comparisons; both prompt paths agree). R6 has no held-out LoCoMo
   set, so this stays a development result.
2. On LoCoMo-10 it matches Hindsight: +0.39 points [−0.80, +1.57] against
   Hindsight with observations on, its best fresh configuration here, and
   −0.52 [−2.68, +1.78] against Hindsight's published AMB contexts read with
   the same reader and judge.
3. On LME-84 (category-balanced; not a LongMemEval score; development set), it
   beats Honcho (+16.67 points [+7.14, +26.19]) and Mastra OM (+32.14 [+21.43,
   +42.86]) under the pre-registered rule (Holm-adjusted p 0.009 and 0.002). A
   held-out LongMemEval split must confirm this before it is a headline claim.
4. On LME-84 it matches MemMachine (0.00 points [−5.95, +7.14]), Hindsight
   (+7.14 [0.00, +14.29]) and Mem0 (+7.14 [0.00, +14.29]).
5. Against Cognee on LME-84 the registered difference is +17.86 points [+8.33,
   +27.38]. With Cognee's 8 second-failure questions scored as read it is
   +10.71 [+1.19, +20.24] (Holm-adjusted p 0.18), so the pre-registered rule
   does not allow a 'beats'.
6. On BEAM-100K (judge validated on LongMemEval only; development set), it
   beats Mem0 (+4.17 points [+1.07, +7.27]), MemMachine (+5.42 [+2.02, +8.75]),
   Honcho (+9.70 [+7.49, +11.81]), Mastra OM (+14.80 [+10.73, +18.71]) and
   Graphiti (+27.88 [+23.39, +32.17]) under the pre-registered rule.
   BEAM-500K/1M (R7b) must confirm this before it is a headline claim.
   Against Mem0 and MemMachine this rests on the registered cluster-bootstrap
   p (Holm 0.038 and 0.025). An exact 20-conversation sign-flip test, which
   was not registered, gives 0.118 and 0.059.
7. On BEAM-100K its leads over Hindsight with observations on (+4.55 points
   [+1.63, +7.91]) and over Cognee (+8.86 [+6.26, +11.42]) are not confirmed
   on AMB's native prompt path (+2.93 [−0.40, +6.36]; +2.66 [−1.10, +6.45]),
   so neither is a 'beats'.
8. It trails none of the seven systems: no primary interval lies below zero in
   the 20 registered comparisons that were run. Graphiti was not run on LME-84
   (cost; user decision 2026-10-11).
9. Of the 21 pre-registered comparisons (development sets), 13 are 'beats', 4
   'matches', 3 lead on the primary path without meeting every 'beats'
   condition, none 'trails', and one was not run.

### Sensitivities

None of these changes a registered verdict. Two of them qualify the
BEAM-100K results against Mem0 and MemMachine.

- **Exact tests.** With an exact test on every dataset instead of the
  registered bootstrap p on LME-84 and BEAM-100K, the BEAM-100K comparisons
  with Mem0 and MemMachine would have Holm-adjusted p 0.118 and 0.059, and
  would not be "beats". Every other verdict stays.
- **Bonferroni intervals** (reported, not part of the rule). Every "beats" has
  a Bonferroni interval above zero except BEAM-100K against Mem0
  ([−0.45, +8.88]) and against MemMachine ([−0.00, +10.60]).
- **The failed BEAM-100K request.** Scoring the one request that failed at the
  metering proxy as correct moves no second-path interval across zero.
- **LoCoMo-10 without its 99 audited gold-answer errors.** No verdict changes;
  against Hindsight with observations on, the difference is +0.62
  [−0.67, +1.81].

### Method

The protocol was written on 11 October 2026, before any scored answer was
read or judged, and each later change is a dated amendment (19 in all). The
Graphicle arm's configuration was registered on 13 October 2026, before its
first model call. Two amendments were decided after its weak-reader scores
had been seen: one changed the order of the remaining runs, the other added
two model servers. Neither changed a request, prompt, setting, statistic or
decision rule.

- **Datasets.** All three are development sets for Graphicle (see the labels).
  - LoCoMo-10: all 1,540 questions in 10 conversations.
  - LME-84: 84 LongMemEval-S questions, 12 from each of 7 strata.
  - BEAM-100K: 400 questions in 20 conversations, scored by rubric.
- **Same models.** One model, Qwen3.8-27B-FP8 served by vLLM, does every
  model step: it is every system's internal LLM, it reads each retrieved
  context and answers, and it judges each answer. bge-small-en-v1.5 is every
  system's embedder: the other systems called BAAI's weights on the shared
  embedding server, and Graphicle ran an int8 ONNX export of it in its own
  process (see the labels). These open models stand in for the hosted models
  that published benchmark runs use.
- **The judge.** The judge was validated against GPT-4o on LongMemEval answers
  only. A seven-case calibration ran before each judging session, as a smoke
  test.
- **Ingestion.** Each system ingests the documents that the Agent Memory
  Benchmark's loaders produce, with opaque identifiers, following its vendor's
  own harness. No system sees a question before its conversation is ingested,
  and none sees answers, evidence or categories.
- **Two readers and two prompt paths.**
  - The weak reader (primary) does not think, runs at temperature 0 and
    answers in at most 4,096 tokens.
  - The strong reader (secondary) thinks, with up to 32,768 tokens; the judge
    sees only its final answer.
  - Each context is read through common prompts and, as a second path,
    through the Agent Memory Benchmark's own prompts.
- **Systems and versions.**
  - Mem0 OSS 2.2.1 (`mem0`, with dates; `mem0-nodate` was the other declared
    configuration)
  - Hindsight 0.10.2 (`hindsight` and `hindsight-obs`, observations on)
  - Graphiti (graphiti-core 0.30.2)
  - MemMachine 0.4.0
  - Mastra Observational Memory (@mastra/memory 1.36.0)
  - Cognee 1.6.3
  - Honcho 3.2.2

  A system with two declared configurations is compared, on each dataset,
  in the one that scored higher there on the weak reader.
- **Calibration.** One arm replays Hindsight's published contexts through the
  same reader and judge. It checks whether this setup handicaps that system.
- **Statistics.**
  - The primary comparisons are Graphicle R5 with the evaluation renderer
    render_v2.py (not shipped) against each system on each dataset, 21 in
    all, Holm-corrected.
  - Every question stays in the denominator, and a failure scores 0.
  - Intervals come from paired cluster bootstraps (10,000 draws), clustered
    by conversation (by question on LME-84).
  - p values come from an exact sign-flip test over the 10 conversations on
    LoCoMo-10, and from the two-sided cluster bootstrap on LME-84 and
    BEAM-100K.
- **The claim rule** is the one under the comparison table above.
- **Records.** The protocol with its amendments, the per-question verdicts
  and the analysis scripts are not in this repository yet; this page
  summarizes them.

### Calls and tokens

- **What is counted.** LLM calls and tokens are each system's own model calls
  while it ingests, counted by the evaluation's metering proxy and divided
  among the conversations being ingested at the time. Every system ingested
  its three datasets at once (Cognee one at a time), so the per-dataset
  figures are estimates; each system's total is exact. Query-time calls are
  separate: Mastra OM makes one LLM call per question, Graphicle makes none,
  and the other systems make embedding calls only. The evaluator's own calls
  (reading and judging) are not a system's cost.
- **Not a cost claim.** The other systems shared 2 to 4 GPUs and one metering
  proxy while they ran; Graphicle's arm ran alone. These figures describe the
  runs; they are not a speed or cost comparison.
- **Graphicle's calls and tokens** come from the same meter (tag
  `tag-graphicle`) and were divided by the same analysis code as the other
  systems': 4,456 LLM calls in all, with 16.56M prompt and 6.13M completion
  tokens, all while it ingested, and every one answered (HTTP 200); 4 of
  them ended at their output limit. It made no embedding calls to the model
  servers, because it embeds in its own process.

## Agent memory: how the retrieval defaults were chosen

Model-free retrieval runs chose the defaults. They ask whether the evidence
packet contains the gold evidence, not whether an answer is right. The
comparison above measured answers with a pre-release build and an evaluation
renderer that is not shipped, so **the answer accuracy of the shipped
defaults as they are has not been measured**. The run records of this
section are not published yet, so this page gives the decisions and not the
numbers.

- **On by default: `stem`, `speaker: "auto"` and `pack_focus`.** Paired runs
  on LongMemEval-S and LongMemEval-M questions and on 120 questions about two
  code repositories (httpx and click) chose them. Each option was added to the
  one before it and tested with McNemar's test. These datasets are development
  sets: the options were tuned on them.
- **Dense arm at weight 0.5.** On BEAM-100K units 5–20, which were held out
  from the retrieval-weight tuning only (BEAM-100K is otherwise a development
  set; see the [labels](#labels)), a dense arm at weight 0.5 tied stemmed BM25
  alone, so the dense arm stays optional, at weight 0.5 (it was 2).
- **No cross-encoder by default.** A local cross-encoder reranker failed its
  held-out check, whose pass rule was fixed before it ran, and it took many
  times as long per question. It stays off, and it is not recommended for one
  conversation's history.
- **`note_keys: "merge"`**, which keys passages by the notes that quote them,
  raised evidence recall in the held-out check. It is kept, but off until its
  effect on answers is measured.

## Reproducing

The memory benchmark kit is in `bench/memory`. It runs the real `graphicle`
program and contains the recall runners, an Agent Memory Benchmark provider and
the scorers. The datasets are not included; you pass their paths. The harness
of the comparison with seven memory systems and its evaluation renderer are not
part of this repository. The checks of the code-graph engine run from
`crates/graphicle-graph/` and need the pinned Python Graphify 0.9.88 reference;
see `crates/graphicle-graph/README.md`, section "Verification".
