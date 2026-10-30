# SWE-bench Pro: gold-file recall and change localization

SWE-bench Pro's public set ([ScaleAI/SWE-bench_Pro](https://huggingface.co/datasets/ScaleAI/SWE-bench_Pro),
V2: 642 tasks in 11 repositories) used as an external test of finding the files a fix changes. The method
was fixed before any number was measured. The settings of `graphicle pack` are tuned only on the development
repositories of the whole-repository benchmark (`../whole-repo`); SWE-bench Pro is only measured. One caveat:
the line-level diagnosis of 0.1.9 on SWE-bench Pro (`e1l.py`) chose the directions of 0.1.10's pack tuning
(whole files, excerpts, ranking and HTML files), though not its settings, so 0.1.10's results on it are not an
untouched test. The clean check is the whole-repository benchmark's held-out repositories, which are never
tuned on.

Results: [results-e1.md](results-e1.md) (gold files, Graphicle 0.1.9) and [results-e1l.md](results-e1l.md)
(gold lines, 0.1.9 and the 0.1.10 candidate build).

## The tasks

- **Data:** config `default`, split `test`, revision `2d52cb3df914a3fcf80c7f66738b3a88ae37fc50`
  (`data.py` checks the file's sha256). `tasks.jsonl` lists each task's repository, base commit, gold files,
  the files its reference patch creates, the sha256 of its problem statement and whether the statement names
  a gold path. The statements themselves are not in this folder; `data.py` rebuilds them.
- **The question** is the problem statement alone. The dataset's `requirements` and `interface` fields are not
  used, since they often name the files and functions to change.
- **Gold files** are the files the reference patch changes that exist at the base commit. Files it creates
  cannot be in any context and are not gold; the 9 tasks whose patch only creates files are left out (633
  remain).
- **The repository state** is the base commit, without the test patch.

## Recall without a model (`e1.py`)

For each task: check out the base commit, build Graphicle's index (timed), count every file with one
tokenizer, then fill each budget with three contexts and record which gold files are inside.

| Context | How it is built |
|---|---|
| pack | `graphicle pack "<problem statement>" --budget B --layout standard --tokenizer <tokenizer.json>`; a gold file is inside when the pack holds a passage of it (rows also count gold files held whole) |
| BM25 | file-level BM25, the SWE-bench paper's retrieval baseline (`../whole-repo/bm25.py`): whole files in score order, each if it still fits |
| truncation | the repository's files in a fixed order (source, tests, docs), each if it still fits |

Budgets are 16K, 32K, 64K, 128K and 256K tokens, plus 1M where a repository holds more than 1M tokens. BM25
and truncation read the same files: source, tests, docs and changelogs, without vendored, generated or binary
files (`../whole-repo/paths.py`). The report gives the share of tasks with every gold file inside (all-gold)
and with at least one (any-gold), with 95% bootstrap intervals over tasks.

## Line-level recall without a model (`e1l.py`)

Does a context hold the lines each fix changes, and not only some text of the files? `e1l.py gold` takes each
task's gold lines from its reference patch: the lines of the base commit that the patch removes or replaces (its
`-` lines) and, for a hunk that only adds lines, the line before each run it adds. Every gold line is checked
against the file at the base commit. `e1l.py run` then builds the three contexts at E1's budgets and records
which gold lines each holds: for the pack, the lines that its whole files and excerpts cover (lines shown only
in an outline never count); for BM25 and truncation, every line of a file they hold. `e1l_report.py` gives, per
budget, (a) the share of tasks with every gold line inside, (b) the mean share of gold lines inside and (c) the
share of gold files with all their gold lines inside, each against BM25 with intervals paired by task, and the
state of each gold file the pack misses. `e1l_compare.py` sets two builds side by side, paired by task, with E1's
file-level readings beside the line-level ones.

## Answers from Muse Spark (the answer runs)

100 tasks stratified by repository (`sample.py`, seed 20261027; `sample-e2.json`), three arms: no context,
BM25 at 64K and the pack at 64K, with the contexts measured above (`e2.py`). The prompt, the scorer (hit@1,
hit@5) and the paired tests are those of the whole-repository benchmark (`../whole-repo`). Every call is logged
with the tokens the API reports, and a run stops before a call that could pass its spending cap.

## The billed cache meter (`c4c.py`)

One kubernetes pack of 1M tokens in the cache layout: a question-independent prefix of 90% that every call
shares, then each question's own suffix. Muse Spark answers 20 questions on it and Muse Wonder 10, one call at a
time under one cache key. Each call's input, cached and output tokens are the API's own usage fields, and its
dollars are those fields at the published prices, so the first call can be compared with the follow-ups.

## Reproduce

Python 3.12 with `pyarrow` and `tokenizers`, git, and a Graphicle binary built with `local-inference`.
`node_setup.sh` makes the environment in a work folder (a virtual environment, the data, the binary and a bare
clone of each repository); it needs `WORK`, `BUILD` (a name for the build, such as its release version), for the
binary step `GRAPHICLE` (the program to copy), and for the g1 step `BENCH` (the whole-repository benchmark's work
folder) and `TOK` (the Muse Spark Open `tokenizer.json` that its manifests were counted with). Then:

```sh
python3 e1.py run --work WORK --graphicle graphicle --tokenizer tokenizer.json --ids sample-e2.json --contexts
python3 e1.py run --work WORK --graphicle graphicle --tokenizer tokenizer.json --order shuffled
python3 e1_report.py --work WORK --md e1.md
python3 e1l.py gold --work WORK --parquet WORK/raw/test-00000-of-00001.parquet
python3 e1l.py run --work WORK --graphicle graphicle --tokenizer tokenizer.json
python3 e1l_report.py --work WORK --md e1l.md
python3 e2.py --work WORK --arm noctx --out WORK/out/e2/noctx --key-file KEY   # then --arm bm25, --arm pack
python3 c4c.py prepare --work WORK --graphicle graphicle --tokenizer tokenizer.json --texts TEXTS
python3 c4c.py run --work WORK --out WORK/out/c4c --texts TEXTS --key-file KEY
```

To measure another Graphicle build, `e1.py run ... --run NAME --reuse e1` builds every index and pack again
with it and takes the earlier run's BM25 and truncation rows as they are: those two arms never call Graphicle,
so a new build cannot change them. `e1_report.py --run NAME` gives its tables, and
`e1_compare.py --a e1 --b NAME` sets the two builds side by side. At line level, `e1l.py run ... --run NAME
--reuse e1l` measures the pack of another build in the same way, and `e1l_compare.py --base e1l --new NAME` sets
the two side by side.

`live.sh` runs the model runs (E2, the cache meter and the whole-repository grid) through one spend log with
caps; it needs `WORK`, `BUILD`, `TOK` (the Muse Spark Open `tokenizer.json`, which is not part of this
repository) and `KEY_FILE`, and for the `c4c`, `g1-pilot` and `g1` steps `TEXTS` (the whole-repository
benchmark's question texts).

`test_e1.py`, `test_e1l.py`, `test_e2.py` and `test_c4c.py` test them without a model (the last two on
`../whole-repo/mock_server.py`).
