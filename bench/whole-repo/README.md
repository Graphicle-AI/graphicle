# Whole-repository change localization

A benchmark of finding the files a real fix changed, in eight public repositories that are each far larger
than a 1M-token context window. It compares Graphicle's `graphicle pack` with truncation of the repository,
with the same model searching the repository through tools (the search-only agent), and with no repository
context. The method, the questions and the scorer were fixed before any model result was seen. The report is
[docs/benchmarks/whole-repo.md](../../docs/benchmarks/whole-repo.md). Line-level recall without a model (`lines.py`),
0.1.9 against the 0.1.10 candidate build: [results-lines.md](results-lines.md).

| Role | Repository |
|---|---|
| Development (used to tune `graphicle pack`) | kubernetes/kubernetes, home-assistant/core |
| Development, added 27 Oct 2026 for TypeScript, Java and C++ (before any tuning on them) | angular/angular, keycloak/keycloak, godotengine/godot |
| Held out (never used for tuning; run once per release candidate) | pytorch/pytorch, microsoft/vscode, elastic/elasticsearch |

The three newer development repositories were mined with the same method and filters, 40 questions each
(`repos.json`; the selection seed is `selection_seed` plus the repository's place in that list).

## The question sets

`questions/questions-<name>.jsonl` holds one question per line:

| Field | Meaning |
|---|---|
| `id` | `<name>-<pull request number>` |
| `repo`, `S` | the repository and the snapshot commit the question is asked at |
| `issue_url`, `pr_url`, `merged_at` | the issue, the pull request that fixed it and when it was merged |
| `gold` | the source files the fix changed that exist at S |
| `text_sha256` | the sha256 of the question text (UTF-8) |
| `names_gold_path` | the question text names a gold file by a path of at least two parts |

The issue texts are not in the repository. `fetch_texts.py` rebuilds them from GitHub (the issue's title and
body as first filed, from its edit history, with HTML comments and empty template headings removed) and
checks each against its hash. `questions/funnel.json` counts what each selection filter removed;
`mine.py` documents the filters.

## Files

| File | What it does |
|---|---|
| `repos.json`, `snapshots.json` | the repositories, the snapshot rule and each snapshot commit S |
| `paths.py` | the file classes: source, test, docs, changelog, generated, vendored |
| `fetch_prs.sh`, `fetch_issues.sh` | read-only GitHub queries for the candidate pull requests and their issues |
| `mine.py`, `issue_text.py` | the filters and the question text |
| `checkout.py` | each repository at S |
| `index.py` | `graphicle sync` of each checkout, with its time and size |
| `tokens.py` | the token counter shared by every budgeted arm, repository sizes, truncation |
| `pack_arm.py` | the pack arm's context (`graphicle pack`, in the standard or the cache layout) |
| `bm25.py` | file-level BM25, the SWE-bench paper's retrieval baseline: whole files in score order up to a budget |
| `agent_tools.py` | the search-only agent's tools: grep, find, list_dir, read (also as one deferred namespace for Meta's tool search) |
| `run.py` | runs one arm against an OpenAI-compatible API (Meta's Responses API or chat completions), with the cache key, a global spend log and hard spend caps |
| `mock_server.py` | a mock OpenAI-compatible server (chat completions and responses, with prompt caching per cache key) for testing without a model |
| `test_runner.py` | end-to-end tests of the runner, the caps, the spend log and the estimate on the mock |
| `score.py` | hit@k, all-gold@5, MRR, context recall, tokens, cost; McNemar tests and bootstrap intervals |
| `recall.py` | context recall without a model, for every question and budget |
| `lines.py` | line-level recall without a model: each question's pull request diff placed on S, then the share of the changed lines each context holds (the pack, truncation, BM25), and two builds compared question by question |
| `estimate.py` | a planned grid's tokens and cost with and without the prompt cache, before any run; the registered grids |
| `report.py` | the report's data file and page; the chart is `docs/benchmarks/whole_repo_chart.py` |
| `prices.json` | the model prices used for cost, with their date and source |
| `caps.json` | the global spending limits per provider |

## Reproduce

Python 3.12 with `tokenizers` (for an exact tokenizer) and `matplotlib` (for the chart), git, and a Graphicle
binary; `graphicle pack --tokenizer` needs a build with the `local-inference` feature. WORK is a working folder
with room for the checkouts and indexes (about 40 GB). The exact counts use the model's own `tokenizer.json`,
which is not part of this repository.

```sh
python3 checkout.py WORK
python3 fetch_texts.py --questions questions --out WORK/texts
python3 index.py --binary graphicle --work WORK
python3 tokens.py sizes --work WORK --out WORK/sizes.json [--tokenizer tokenizer.json]
python3 estimate.py --questions questions --texts WORK/texts --registered
export MODEL_API_KEY=...   # or --key-file FILE; BASETEN_API_KEY for --profile baseten
python3 run.py --arm pack --budget 128K --work WORK --questions questions --texts WORK/texts \
    --out WORK/runs/pack-128K --profile meta --spend-cap 20 --graphicle graphicle [--tokenizer tokenizer.json]
python3 run.py --arm trunc --budget 128K ...   # and --arm agent, --arm noctx, --arm pack-cache
python3 score.py --runs WORK/runs/* --out WORK/score.json
python3 recall.py --work WORK --questions questions --texts WORK/texts --graphicle graphicle --out WORK/recall
python3 lines.py gold --work WORK --questions questions --diffs WORK/diffs --out WORK/gold-lines.jsonl --fetch
python3 lines.py run --work WORK --questions questions --texts WORK/texts --gold WORK/gold-lines.jsonl \
    --graphicle graphicle --data WORK/data --out WORK/lines --tokenizer tokenizer.json --arms pack,trunc
python3 lines.py report --rows WORK/lines/lines-rows.jsonl
python3 report.py --sizes WORK/sizes.json --index-dir WORK/index --score WORK/score.json \
    --recall WORK/recall/recall-summary.json --json ../../docs/benchmarks/whole-repo.json \
    --md ../../docs/benchmarks/whole-repo.md
```

`--profile meta` (the default) sends to `https://api.meta.ai/v1` with the Responses API and `muse-spark-1.3`;
`--profile baseten` sends to Baseten's chat completions with Muse Spark Open. Each call carries a cache key
(repository, snapshot, arm, budget and the hash of the shared prefix, never question text), sent as
`prompt_cache_key` with `prompt_cache_retention: "24h"` on Meta and as the `x-session-affinity` header on
Baseten. The questions of one repository run back to back, the first one alone, so the others can be served
from the cache.

Every call is appended to one spend log (`WORK/spend/spend-log.jsonl`: tokens, cached tokens, seconds, cost).
`run.py` refuses to start when the provider's logged total plus the run's estimate would pass the provider's
limit in `caps.json`, and stops before any call that could take its spending past `--spend-cap` (default $20)
or past what is left of that limit, counting what earlier runs into the same folder spent.

To test everything without a model, run `python3 test_runner.py` (it starts `mock_server.py` itself), or
start `mock_server.py` and use `--profile mock --base-url` pointing at it, with any key.
