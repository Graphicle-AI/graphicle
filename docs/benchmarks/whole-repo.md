# Whole-repository change localization past 1M tokens

Can a model find the files a real fix changed, in a repository far larger than its context window? This benchmark asks it for five public repositories of 47 to 104 million tokens each, with 170 questions taken from real issues and the files their fixes changed. It compares Graphicle's `graphicle pack` with plain truncation of the repository, with the same model searching the repository through tools, and with no repository context at all. The method was fixed before any result was seen, and the results are published as they came out.

## Results

Not run yet. The questions, arms, prompts and scorer are fixed and committed; the model runs come next.

## Context recall without a model

For every question and budget, with no model involved: the share of the question's gold files that are inside the context, for the pack and for truncation filled to the same budget, with tokens counted with the Muse Spark Open tokenizer (Hugging Face revision 4239e4db). The pack holds small files whole and large ones as excerpts and as outlines of their symbols, and a gold file counts as inside when any of its text is in the pack; truncation holds whole files only. "Some inside" is the share of questions with at least one gold file inside. Each table is one run, named by the pack build it used.

### A pre-release build of the pack (26 Oct, before the event): held-out repositories

| Repository | Role | Budget | Questions | Pack: gold files inside | Truncation: gold files inside | Pack: some inside | Truncation: some inside |
|---|---|---|---|---|---|---|---|
| pytorch | held-out | 64K | 30 | 60.3% | 0.0% | 66.7% | 0.0% |
| pytorch | held-out | 128K | 30 | 71.9% | 0.0% | 76.7% | 0.0% |
| pytorch | held-out | 256K | 30 | 65.3% | 0.0% | 70.0% | 0.0% |
| pytorch | held-out | 512K | 30 | 71.9% | 0.0% | 76.7% | 0.0% |
| pytorch | held-out | 1M | 30 | 80.0% | 0.0% | 83.3% | 0.0% |
| vscode | held-out | 64K | 30 | 63.3% | 3.3% | 63.3% | 3.3% |
| vscode | held-out | 128K | 30 | 64.2% | 3.3% | 66.7% | 3.3% |
| vscode | held-out | 256K | 30 | 64.2% | 3.3% | 66.7% | 3.3% |
| vscode | held-out | 512K | 30 | 70.8% | 3.3% | 73.3% | 3.3% |
| vscode | held-out | 1M | 30 | 74.2% | 6.7% | 76.7% | 6.7% |
| elasticsearch | held-out | 64K | 30 | 43.8% | 0.0% | 56.7% | 0.0% |
| elasticsearch | held-out | 128K | 30 | 54.7% | 0.0% | 66.7% | 0.0% |
| elasticsearch | held-out | 256K | 30 | 54.7% | 0.0% | 66.7% | 0.0% |
| elasticsearch | held-out | 512K | 30 | 61.2% | 0.0% | 70.0% | 0.0% |
| elasticsearch | held-out | 1M | 30 | 76.2% | 0.0% | 83.3% | 0.0% |
| **all held-out** | held-out | 64K | 90 | 55.8% | 1.1% | 62.2% | 1.1% |
| **all held-out** | held-out | 128K | 90 | 63.6% | 1.1% | 70.0% | 1.1% |
| **all held-out** | held-out | 256K | 90 | 61.4% | 1.1% | 67.8% | 1.1% |
| **all held-out** | held-out | 512K | 90 | 68.0% | 1.1% | 73.3% | 1.1% |
| **all held-out** | held-out | 1M | 90 | 76.8% | 2.2% | 81.1% | 2.2% |

### A pre-release build of the pack (26 Oct, before the event): development repositories (used for tuning)

| Repository | Role | Budget | Questions | Pack: gold files inside | Truncation: gold files inside | Pack: some inside | Truncation: some inside |
|---|---|---|---|---|---|---|---|
| kubernetes | development | 64K | 40 | 85.0% | 0.0% | 90.0% | 0.0% |
| kubernetes | development | 128K | 40 | 88.4% | 0.0% | 92.5% | 0.0% |
| kubernetes | development | 256K | 40 | 89.2% | 0.0% | 95.0% | 0.0% |
| kubernetes | development | 512K | 40 | 91.8% | 0.0% | 97.5% | 0.0% |
| kubernetes | development | 1M | 40 | 92.4% | 1.5% | 97.5% | 2.5% |
| home-assistant | development | 64K | 40 | 94.2% | 0.0% | 95.0% | 0.0% |
| home-assistant | development | 128K | 40 | 94.2% | 3.3% | 95.0% | 5.0% |
| home-assistant | development | 256K | 40 | 94.2% | 5.8% | 95.0% | 7.5% |
| home-assistant | development | 512K | 40 | 100.0% | 8.3% | 100.0% | 10.0% |
| home-assistant | development | 1M | 40 | 100.0% | 12.5% | 100.0% | 12.5% |
| **all development** | development | 64K | 80 | 89.6% | 0.0% | 92.5% | 0.0% |
| **all development** | development | 128K | 80 | 91.3% | 1.7% | 93.8% | 2.5% |
| **all development** | development | 256K | 80 | 91.7% | 2.9% | 95.0% | 3.8% |
| **all development** | development | 512K | 80 | 95.9% | 4.2% | 98.8% | 5.0% |
| **all development** | development | 1M | 80 | 96.2% | 7.0% | 98.8% | 7.5% |

## Cost per question with the prompt cache

**All numbers in this section are computed at list prices, not billed.** Runs with the model will replace them with billed costs.

Meta's API serves the longest prefix a prompt shares with an earlier prompt from its cache, and bills it at the cached-input price (Muse Spark 1.3 $1.25 per million input tokens, $0.15 cached; Muse Wonder $4.00 per million input tokens, $0.20 cached; prices of 2026-10-26). A context whose first part is the same for every question about a repository pays the full input price once; every follow-up question pays the cached price on that part and the full price only on the rest. Truncation's dump is the same for every question, so it is cached the same way. These are input costs; output is billed on top ($4.25 per million on Muse Spark 1.3), ($20.00 per million on Muse Wonder).

The shared prefix and the question-specific tokens are the means over the questions of the development repositories (kubernetes, home-assistant) for each context and budget (a pre-release build of 0.1.9), counted with the Muse Spark Open tokenizer. A first question pays for both, plus the question and the prompt template after the context.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="../assets/whole-repo-cost-dark.svg">
    <img src="../assets/whole-repo-cost-light.svg" alt="Input cost per follow-up question at a 1M-token budget, without and with the prompt cache, computed at list prices, not billed. Graphicle pack, standard layout: Muse Spark 1.3 $1.27 without the cache, $1.27 with it, Muse Wonder $4.06 without the cache, $4.06 with it; Graphicle pack, cache layout, 50% prefix: Muse Spark 1.3 $1.17 without the cache, $0.610 with it, Muse Wonder $3.74 without the cache, $1.81 with it; Graphicle pack, cache layout, 70% prefix: Muse Spark 1.3 $1.21 without the cache, $0.433 with it, Muse Wonder $3.88 without the cache, $1.19 with it; Graphicle pack, cache layout, 90% prefix: Muse Spark 1.3 $1.26 without the cache, $0.252 with it, Muse Wonder $4.02 without the cache, $0.551 with it; Graphicle pack, cache layout, 95% prefix: Muse Spark 1.3 $1.26 without the cache, $0.204 with it, Muse Wonder $4.04 without the cache, $0.383 with it; Graphicle pack, stable layout: Muse Spark 1.3 $1.27 without the cache, $0.153 with it, Muse Wonder $4.06 without the cache, $0.206 with it; Truncation: Muse Spark 1.3 $1.27 without the cache, $0.153 with it, Muse Wonder $4.07 without the cache, $0.206 with it.">
  </picture>
</p>

| Budget | Context | Shared prefix (tokens) | Question-specific (tokens) | Gold files inside (development) | Muse Spark 1.3: first question | Muse Spark 1.3: each follow-up | Muse Spark 1.3: mean over 20 | Muse Wonder: first question | Muse Wonder: each follow-up | Muse Wonder: mean over 20 |
|---|---|---|---|---|---|---|---|---|---|---|
| 128K | Graphicle pack, standard layout | 0 | 131,250 | 91.3% | $0.164 | $0.164 | $0.164 | $0.525 | $0.525 | $0.525 |
| 128K | Graphicle pack, cache layout, 50% prefix | 65,376 | 61,283 | 89.6% | $0.158 | $0.086 | $0.090 | $0.507 | $0.258 | $0.271 |
| 128K | Graphicle pack, cache layout, 70% prefix | 91,562 | 37,852 | 86.8% | $0.162 | $0.061 | $0.066 | $0.518 | $0.170 | $0.187 |
| 128K | Graphicle pack, cache layout, 90% prefix | 117,686 | 13,284 | 80.5% | $0.164 | $0.034 | $0.041 | $0.524 | $0.077 | $0.099 |
| 128K | Graphicle pack, cache layout, 95% prefix | 124,242 | 7,019 | 65.7% | $0.164 | $0.027 | $0.034 | $0.525 | $0.053 | $0.077 |
| 128K | Graphicle pack, stable layout | 130,792 | 781 | 6.7% | $0.164 | $0.021 | $0.028 | $0.526 | $0.029 | $0.054 |
| 128K | Truncation | 131,066 | 781 | 1.7% | $0.165 | $0.021 | $0.028 | $0.527 | $0.029 | $0.054 |
| 1M | Graphicle pack, standard layout | 0 | 1,013,783 | 96.2% | $1.27 | $1.27 | $1.27 | $4.06 | $4.06 | $4.06 |
| 1M | Graphicle pack, cache layout, 50% prefix | 506,857 | 427,440 | 95.9% | $1.17 | $0.610 | $0.638 | $3.74 | $1.81 | $1.91 |
| 1M | Graphicle pack, cache layout, 70% prefix | 709,619 | 261,605 | 92.9% | $1.21 | $0.433 | $0.472 | $3.88 | $1.19 | $1.32 |
| 1M | Graphicle pack, cache layout, 90% prefix | 912,369 | 92,025 | 94.4% | $1.26 | $0.252 | $0.302 | $4.02 | $0.551 | $0.724 |
| 1M | Graphicle pack, cache layout, 95% prefix | 963,066 | 47,682 | 92.1% | $1.26 | $0.204 | $0.257 | $4.04 | $0.383 | $0.566 |
| 1M | Graphicle pack, stable layout | 1,013,762 | 781 | 18.2% | $1.27 | $0.153 | $0.209 | $4.06 | $0.206 | $0.398 |
| 1M | Truncation | 1,015,800 | 781 | 7.0% | $1.27 | $0.153 | $0.209 | $4.07 | $0.206 | $0.399 |

Every question about one repository at one budget got the same prefix bytes in the cache and stable layouts (one sha256 per repository, budget and layout). Gold files inside: the share of each question's gold files with any of their text in the context (in a pack, whole or as excerpts or outlines), the same measure as in the context recall section.

The 256K and 512K rows are in the data file [whole-repo.json](whole-repo.json). Whether a follow-up is served from the cache is up to the API: the 24-hour retention is a hint, and a prompt that leaves the cache pays the first-question price again.

## Repositories

Each repository is pinned at S, the last commit on its default branch's first-parent line at or before 2026-09-14 00:00 UTC. Meta's model documentation states no knowledge cutoff for Muse Spark, so the date was not moved. Sizes count the tracked text files at S, in tokens counted with the Muse Spark Open tokenizer (Hugging Face revision 4239e4db); in brackets, the estimate of UTF-8 bytes / 4; vendored and generated files are listed in `bench/whole-repo/paths.py`.

| Repository | Role | S | S date (UTC) | Tokens, all | Without vendored and generated | Non-test source | Text files | Index time | Index size |
|---|---|---|---|---|---|---|---|---|---|
| kubernetes/kubernetes | development | `d5cbb79be916` | 2026-09-12 20:05 | 74.4M (68.4M) | 41.4M (39.7M) | 12.0M (12.2M) | 30,388 | 268 s | 5.9 GiB |
| home-assistant/core | development | `09e3474f91be` | 2026-09-13 20:00 | 47.2M (48.0M) | 46.9M (47.8M) | 13.5M (15.3M) | 27,850 | 188 s | 4.4 GiB |
| pytorch/pytorch | held-out | `b8bd7cf750ea` | 2026-09-13 22:40 | 49.8M (48.1M) | 46.3M (44.8M) | 28.7M (27.1M) | 21,685 | 255 s | 4.5 GiB |
| microsoft/vscode | held-out | `3e8601c5f63f` | 2026-09-13 21:23 | 63.2M (59.8M) | 59.1M (56.7M) | 30.6M (29.4M) | 18,541 | 278 s | 5.0 GiB |
| elastic/elasticsearch | held-out | `d8a9ab59389d` | 2026-09-13 20:09 | 103.6M (103.0M) | 99.6M (98.9M) | 30.7M (36.1M) | 47,679 | 599 s | 14.1 GiB |

Index time is the wall time of `graphicle sync` (graphicle 0.1.8 (core)) on 48 cores of a shared machine at low priority, two repositories at a time; index size is its data folder.

## Questions

Each question is a real issue, fixed by a pull request merged into the default branch after S and by 2026-10-25. The question is the issue's title and body as first filed (from GitHub's edit history), with HTML comments and empty template headings removed. The gold answer is the source files the pull request changed that exist at S: no tests, docs, changelogs, generated or vendored files. Only pull requests with 1 to 5 gold files are kept, and only when their change to the gold files applies cleanly to S, so the bug is present in the snapshot. When more questions pass than the target, the target number is drawn with the fixed seed 20261026. The question sets publish the issue and pull request links, the gold files and the sha256 of each question text; `fetch_texts.py` rebuilds the texts and checks them.

| Repository | Pull requests found | No readable linked issue | Over 100 files | Not 1 to 5 source files | Diff not readable | Not 1 to 5 gold files | Does not apply at S | Issue not usable | Issue already used | Passed | Kept | Text names a gold path |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| kubernetes/kubernetes | 89 | 8 | 1 | 26 | 0 | 0 | 4 | 0 | 0 | 50 | 40 | 23 |
| home-assistant/core | 604 | 15 | 1 | 129 | 0 | 8 | 114 | 0 | 4 | 333 | 40 | 10 |
| pytorch/pytorch | 860 | 730 | 0 | 24 | 0 | 0 | 17 | 1 | 0 | 88 | 30 | 9 |
| microsoft/vscode | 715 | 71 | 3 | 180 | 0 | 37 | 234 | 2 | 2 | 186 | 30 | 5 |
| elastic/elasticsearch | 579 | 187 | 1 | 71 | 0 | 0 | 231 | 0 | 3 | 86 | 30 | 2 |

The search found the merged pull requests GitHub links to an issue. For PyTorch, whose merge bot closes pull requests instead of merging them, every landed pull request was read and its closing keywords parsed, so most PyTorch pull requests drop out at the linked-issue step. Questions whose text names a gold file path (a path of at least two parts, such as a stack trace line) are flagged; the results are shown with and without them.

## Arms

Every arm sends one prompt: the context, then the question, in this template:

```text
<context>

Repository <org>/<name> at commit <S>. Issue: <issue text>

List the source files that must change to resolve this issue, most likely first, at most 10. Answer with JSON only, in the form {"files": ["path/to/file", ...]}, with paths relative to the repository root.
```

- **Graphicle pack at B tokens:** `graphicle pack "<issue>" --project <repository> --budget B --format xml`.
- **Graphicle pack, cache layout:** the same with `--layout cache --prefix-share 0.9`: the first 90% of the budget is a prefix that does not depend on the question, byte for byte the same for every question about the repository at S, and the rest is chosen for the question. A model API's prompt cache can then serve the prefix of every question after the first at its cached-input price. With `--prefix-share 1.0` (the stable layout) the whole pack is the same for every question.
- **Truncation at B tokens:** the repository's files in a fixed order up to B: source files, then tests, then docs, each group by path, vendored and generated files left out, each file wrapped with its path like the pack's chunks; a file that would pass the budget is skipped.
- **Search-only agent:** the same model with four tools over the checkout at S: `grep` (at most 100 lines), `find` (at most 200 paths), `list_dir` and `read` (at most 400 lines per call), for at most 30 turns, then the same answer. Every token of every turn is counted.
- **No context:** the question alone, the control for memorisation.

Budgets B: 64K = 65,536, 128K = 131,072, 256K = 262,144, 512K = 524,288, 1M = 1,015,808 tokens (1M leaves 32,768 tokens of the 1,048,576-token window for the question and the answer). Pack and truncation are measured in the same tokenizer (counted with the Muse Spark Open tokenizer (Hugging Face revision 4239e4db)).

## Metrics

- **hit@k:** a gold file is among the first k files of the answer (k = 1, 3, 5). **All gold in top 5:** every gold file is among the first 5. **MRR:** the mean of 1/rank of the first gold file (0 when none of the 10 is).
- **Context recall:** the share of gold files whose text is inside the context; for the agent, the share it read.
- **Tokens and cost:** from the API's usage fields; cost from the price table in `prices.json` (2026-10-26, https://dev.meta.ai/docs/pricing-rate-limits).
- **Tests:** McNemar's exact test on hit@5 between arms on the same questions; 95% bootstrap intervals (questions resampled) on the means.

## Model runs, fixed before they ran

These runs, their order, their settings, the tests and the spending limits were fixed on 26 Oct 2026, before any model call. A grid runs only when the one before it has finished and only if it fits the money left; this page says which ones ran.

| Grid | Model | API | Arms, in run order | Questions | Estimated cost with caching (without) |
|---|---|---|---|---|---|
| G1 | Muse Spark 1.3 (`muse-spark-1.3`) | Meta Model API, Responses API | no context, truncation at 128K, pack at 128K, search-only agent | 90 held-out | $34.48 ($74.28) of $45 |
| G2 | Muse Spark 1.3 (`muse-spark-1.3`) | Meta Model API, Responses API | cache-layout pack at 1M (the first 10 questions of each repository) | 30 held-out | $11.48 ($38.64) of $45 |
| G3 | Muse Spark Open (`meta-models/muse-spark-open`) | Baseten Model APIs, chat completions | no context, truncation at 1M, cache-layout pack at 1M, pack at 1M (the first 10 questions of each repository) | 90 held-out | $87.39 ($272.09) of $100 |
| G4 | Muse Wonder (`muse-wonder-1.0`) | Meta Model API, Responses API | cache-layout pack at 128K (the first 10 questions of each repository) | 30 held-out | $6.14 ($18.24) of $45 |

The estimates fill every budget, count 4,000 output tokens per call and assume the search-only agent takes 15 turns that add 2,500 tokens each (`estimate.py --registered`). The 1M arms on Muse Spark 1.3 (G2) and the Muse Wonder subset (G4) run only if the Meta limit still allows them after G1.

- **Caching:** Meta's API caches the longest identical prefix of a prompt and bills it at the cached-input price. Every call sends `prompt_cache_key`, one key per repository, snapshot, arm, budget and prefix hash, and `prompt_cache_retention: "24h"`; on Baseten the same key goes in the `x-session-affinity` header. Truncation, whose dump is the same for every question, gets the same treatment as the pack, so the cost comparison does not favour the pack.
- **Order:** the questions of one repository, arm and budget run back to back in the question file's order, the first one alone, so the later ones can be served from the cache. The search-only agent takes the three repositories in turn, one question each, so a run stopped by its spending limit still covers them evenly.
- **Settings:** reasoning effort medium; at most 8,192 output tokens per call (per turn for the agent); one answer per question; the API's default sampling.
- **Primary comparisons:** hit@5, pooled over the three held-out repositories, by McNemar's exact test: the pack against truncation at 128K and the pack against the search-only agent at 128K (Muse Spark 1.3), and the cache-layout pack against truncation at 1M (Muse Spark Open). Every other comparison is secondary. A question whose call still fails after retries is left out of the means and counted; the hit rates with such questions counted as misses are shown too.
- **Cost:** per question as billed, from the input, cached and output tokens the API reports, and at list prices without any cache.
- **Spending limits:** the runner logs every call (tokens, cached tokens, seconds, cost) and refuses to start a run when the logged total plus the run's own estimate would pass $45 on Meta's API or $100 on Baseten; within a run, no call is sent whose worst case (the whole prompt uncached plus the output limit) would pass the run's own limit or the total one.
- **Held-out repositories:** tuning happens only on kubernetes and home-assistant. Every run on pytorch, vscode or elasticsearch, with or without a model, is logged with its time and pack version before its result is read, and no held-out result changes the pack. The context recall runs once per release candidate; each hosted grid runs once, with one pack version, and a run stopped by an error or a spending limit is resumed, never repeated. The first held-out run is the context recall of the pack as it was built before 26 Oct 2026.

## Limitations

- Five repositories and 170 questions: differences of a few points are within the noise; the intervals say how much.
- File-level localization only, scored by exact path; a fix can be correct in other files than the ones the pull request changed.
- The gold files come from what the fix changed, which is not always where the bug was reported.
- Issues filed before S can be in a model's training data; the no-context arm measures how much that helps.
- Pull requests whose changes do not apply at S were dropped, which favours fixes to code that did not move in the weeks after S.

## Reproduce

The scripts and question sets are in [`bench/whole-repo/`](../../bench/whole-repo/). In order: `checkout.py` (the snapshots), `fetch_texts.py` (the question texts, checked against their hashes), `index.py`, `tokens.py sizes`, `run.py` once per arm (with `--spend-cap`), `score.py`, `recall.py` (with `--layouts` for the cost section) and `report.py`; the charts come from `whole_repo_chart.py` and `whole_repo_cost_chart.py` in this folder. `estimate.py` prints a grid's tokens and cost before it runs.
