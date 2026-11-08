# Graphicle 0.1.11 against Graphify 0.9.100 (protocol, 1 Nov 2026)

Re-run of the three published code-graph comparisons in docs/benchmarks.md (at commit 759b7b9, file sha256
8a198cdc...): "Updates after commits", "Speed on 13 codebases against Graphify 0.9.91" and "Peak memory against
Graphify 0.9.91". The published protocols apply unchanged except where this file lists a change. Results are reported
whatever they show. No model calls, no keys.

## 1. Programs

- **Graphicle 0.1.11.** Rule: the published Linux release archive
  `graphicle-0.1.11-x86_64-unknown-linux-gnu.tar.gz` from github.com/graphicle-ai/graphicle (its sha256 recorded) if
  v0.1.11 is published when the first measured Graphicle run starts; otherwise a build from the release source, an archive of the 0.1.11 release source taken before the release was published (sha256 f111e825...7019), made on the node with
  release.yml's x86_64-unknown-linux-gnu steps: rustup toolchain 1.95.0 (minimal profile), `python3
  crates/graphicle/tools/prepare_parser_sources.py`, then `python3 packaging/release_build.py --out dist
  --expect-version 0.1.11 --require-licenses --max-glibc 2.35` (CARGO_INCREMENTAL=0, no RUSTFLAGS), labelled "built
  from the 0.1.11 release source". One binary pair (`graphicle`, `graphicle-graph`) serves every Graphicle 0.1.11 run;
  the two sources are never mixed. The pair used is named in section 7 before the first measured run.
- **Graphicle 0.1.0.** Its release archive, sha256 41491cd7...ee1de6f (checked, as in the 15 October protocol for updates after commits), binaries
  `graphicle` c502dc51... and `graphicle-graph` 3f734441... (the same files as on 15 Oct).
- **Graphify 0.9.100.** PyPI wheel graphifyy-0.9.100-py3-none-any.whl, sha256 64c5ed6f...e769, equal to PyPI's digest
  (uploaded 2026-11-01T00:53:46Z); own uv venv (uv 0.13.4, uv-managed CPython 3.12.15), installed with graphify-constraints.txt (published here) as constraints (they resolved: no other versions needed); its freeze recorded; `graphify --version` = `graphify 0.9.100`.

## 2. Machine (one node, all runs)

A Databricks GPU_1xH100 node (a cloud VM). `lscpu`: AMD
EPYC 9V84 96-Core Processor; the VM has 40 CPUs (0-39), 1 thread per core, 1 socket, 1 NUMA node; L3 160 MiB in 5
instances of 8 CPUs (0-7, 8-15, 16-23, 24-31, 32-39). Memory 314 GiB (`free -g`), /tmp 3.5 TB. Kernel
6.6.159.1-1.azl3, Ubuntu 24.04.5 userland, glibc 2.39, /usr/bin/python3 3.12.3 for the harness. The H100 is not used
(CUDA_VISIBLE_DEVICES empty). The platform's own processes (Spark driver JVMs, monitoring) keep running and float
across CPUs; nothing else of this measurement runs while a measured run runs.
Published machine: one node with two Intel Xeon Platinum 8480C (96 cores), 8 H100 not used.

**CPU pinning.** Setup (clones, venv, the source build) used CPUs 0-31 and ended before the first measured run.
Step 1: each measured command on CPUs 36-39 (`taskset -c 36-39 nice -n 10`, as published), the driver and its helpers (bash, git, zstd, rtime.py) on 32-35. Steps 2-4: each program run on CPUs 37-39 (3 CPUs), the measuring driver
(rm2.py with speedbench's sampler) on CPU 36, graph digests on 32-35 between runs. Analysis (drift, tables) runs on
0-31 only after the measured runs of its step. Measured runs are strictly one at a time.

## 3. Step 1: updates after commits (the 15 October protocol, sha256 18733031..., with its amendments A1 and A3)

Unchanged: the 3 repositories and the registered ranges (ranges/*.json, published here, sha256 4dfeea64..., 24d92403...,
e0c1d1f2...; pinned 15 Oct; not re-picked; on this node `pick_range.py` at the pinned heads reproduced all three
files exactly), the procedure of section 3 (own replay and scratch worktrees per program, step 0 build + no-op update,
per step checkout + `update .`, then a fresh `extract . --code-only` in a cleaned scratch tree with a new HOME),
timing by rtime.py (wait4 wall time and ru_maxrss), drift by drift.py with A1 (code scope) and A3 (placeholder
re-keying), the summary of section 6, every update counted as it exits.
Changes:
- Programs: Graphicle 0.1.11 (`graphicle graph extract . --code-only`, `graphicle graph update .`), Graphify 0.9.100
  (`graphify extract . --code-only`, `graphify update .`) and Graphicle 0.1.0 (as published) for a version line.
  Per step the three updates run first (0.1.11, 0.1.0, Graphify), then the three fresh builds in the same order.
  harness/replay3.sh, drift3.py and summarize3.py are replay_graph.sh, drift.py and summarize.py with only the third program and this run's paths added.
- One set of 4 CPUs (36-39) instead of two sets in parallel; repositories in the order prometheus, tokio, poetry.
  `GRAPHIFY_MAX_WORKERS=4` (Graphify's default here would be 40); Graphicle sizes itself from the CPU affinity (4).
- The stale-flag part (section 5, Graphicle only) is last priority and runs only if time is left after steps 2-3,
  with Graphicle 0.1.11; any adaptation of its commands is recorded in AMENDMENTS.md before its results are read.
Report: per repository and program, update median / 90th percentile, fresh-build median, highest update peak RSS,
steps that differ from a fresh build, mean difference per step (and after the no-op update), the not-updated control,
non-zero exits; update speed-up = Graphify median / Graphicle median per repository.

## 4. Step 2: speed on 13 codebases (published section; its driver mx91.py with speedbench.py 1df0ce2f...)

Unchanged: the 13 repositories and tags (flask 3.1.3, ripgrep 15.2.0, redis 8.10.2, prometheus v3.15.0, tokio
tokio-1.53.3, react v19.3.0, next.js v16.4.0, pandas v3.0.6, scikit-learn 1.9.1, spring-boot v4.1.1, rails v8.1.4,
kubernetes v1.37.1, vscode 1.141.1) as plain trees whose digests equal the published tree digests (all 13 checked on
this node), used read-only; per repetition and program `extract <src> --code-only --force` into an empty output
folder, a priming `update <src>`, an unchanged `update <src>`; graph caps of 1 GiB for both programs' kubernetes and
vscode updates; speedbench.measure (wait4 wall time incl. start-up, RSS of the process tree every 20 ms, PSS every
500 ms) and speedbench.arm_env (clean environment, 8 workers: GRAPHICLE_/GRAPHIFY_MAX_WORKERS=8, RAYON_NUM_THREADS=8,
PYTHONHASHSEED=0 for Graphify); 3 CPUs per run; the busy gate before each repetition; mx91's timeouts; Graphicle runs
its engine binary `graphicle-graph`, as published. Times are medians of the measured runs; speed-up = Graphify median
/ Graphicle median from unrounded times; node and edge counts of both graphs per cell.
Changes:
- Programs: Graphicle 0.1.11 and Graphify 0.9.100, in one driver (harness/rm2.py = mx91.py with the changes its
  docstring lists).
- Order: one run at a time, alternating: per repository one warm-up (Graphicle, then Graphify; not analysed), then 3
  measured repetitions whose program order rotates (Graphicle first, Graphify first, Graphicle first), as the survey
  harness did. Published: one session per program, chains in parallel, not alternating.
- Runs: one warm-up + 3 measured runs for each of the 11 repositories other than kubernetes and vscode (published: no
  warm-up for react, next.js and spring-boot); 1 measured run, no warm-up, for kubernetes and vscode (as published).
- Repository order: kubernetes first (its extract is the top picture's bar), then flask, ripgrep, redis, prometheus,
  tokio, react, next.js, pandas, scikit-learn, spring-boot, rails, vscode.
- The published "extra runs" with finer memory sampling are step 3.
Report: the published table (times, speed-up, runs, nodes and edges of both programs), the speed-up range over all
measurements, whether Graphicle's slowest run is below Graphify's fastest in each measurement, and the node and edge
count difference range.

## 5. Step 3: peak memory (published section and its sources)

Unchanged measures: Graphicle's peak resident set (wait4 ru_maxrss) and sampled peak PSS (process-tree sum), each the
median of its runs; Graphify's sampled peak PSS of its process tree (main process plus workers), the median of its
runs; a measurement counts as below Graphify only when both Graphicle figures are below Graphify's, and the ratio is
the larger Graphicle figure over Graphify's.
Sampling, as published: PSS every 20 ms for flask, ripgrep, redis, prometheus, tokio, pandas and scikit-learn; every
500 ms for react, next.js, spring-boot, rails, kubernetes and vscode.
- The 500 ms figures come from the step 2 measured runs: PSS every 500 ms is part of the published timing method, so
  sampling them does not change those runs.
- The 20 ms figures come from separate runs after step 2 (series s3): 3 runs per program, no warm-up, alternating
  order, same commands, CPUs and environment. 20 ms PSS reads would change the short step 2 runs, so they are not taken
  there; times from these runs are not used.
Change: both programs' figures for a cell come from the same runs (the published Graphicle figures came from separate
runs of another build).

## 6. Step 4 (optional, only with time left): Graphicle 0.1.0 in the 13-codebase runs (series s4, same method).

## 7. Graphicle 0.1.11 binary used (filled before sealing)

**Built from the 0.1.11 release source.** v0.1.11 was not published (`gh release view v0.1.11 -R
graphicle-ai/graphicle`: "release not found" at 05:31Z and 05:51:38Z). On the node (CPUs 0-31, before any measured run): the release source archive (sha256 f111e825...7019) unpacked; rustup with toolchain 1.95.0 (minimal profile, host
x86_64-unknown-linux-gnu); `prepare_parser_sources.py` (its GitHub bundle, sha256-pinned in the script);
`release_build.py --out dist --expect-version 0.1.11 --require-licenses --max-glibc 2.35` (CARGO_INCREMENTAL=0, no
RUSTFLAGS). It built both programs with the release profile, then exited 1 at its last gate: "graphicle needs glibc
2.39, newer than the 2.35 this release supports" (the node runs Ubuntu 24.04 with glibc 2.39; the workflow builds on
Ubuntu 22.04). That gate checks which glibc the programs need, not what they do, so the two programs of that build
are used, copied into one folder as in the release archive:
- `graphicle` sha256 14c3e9431d5cbe717a8c6c89f3c95217c4ba64107667cbaea95db1ab5d5efc5e, `graphicle 0.1.11 (core)`;
- `graphicle-graph` sha256 4b5b36c7e2607e0a6efc73a5e75df819a97ab6947be4cf5fedf73b3df5da396f, `graphicle-graph 0.1.11`.
They are not byte-identical to the release binaries (C code compiled by the node's compiler; they need glibc 2.39; at
run time both would use the node's glibc 2.39). The build also downloaded the Rust toolchain (static.rust-lang.org)
and the crates of Cargo.lock (crates.io, `--locked`), as release.yml does. Every Graphicle 0.1.11 run uses these two
programs, even if v0.1.11 is published later. Smoke checks (not measured): both harnesses' commands exit 0 with them.

## 8. Outputs, timing and stops

Raw rows and logs on the node under <run>; tables computed there (table1.py, table2.py); one raw
tarball with its sha256 in raw/. A status file records milestones with UTC times from the logs. Everything stops by 00:30
UTC on 2 Nov: in steps 2-4 no repetition starts after RM_CUTOFF_UTC=2026-11-01T23:00:00Z (rm2.py) or after a STOP
file, and step 1 starts no repository after a STOP file; what finished is reported. Changes after sealing go in
AMENDMENTS.md with time and reason before any result they could affect is read.
