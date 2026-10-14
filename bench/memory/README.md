# Graphicle memory benchmark kit

Retrieval and answer benchmarks for Graphicle's memory, run through the real
`graphicle` program. This folder is the first part of the public benchmark
kit: the tools that measure the retrieval options of the runtime README (its
"Retrieval defaults" and "Benchmark kit" sections) and the transport the
answer evaluations use.

| Tool | What it measures | Model calls |
|---|---|---|
| `tools/retrieval_recall.py` | LongMemEval-S/M retrieval recall. Each question's sessions are synced into a fresh store, and the `context` packet is scored against the dataset's support labels: session recall, all-sessions recall, turn coverage and tokens, with McNemar tests for `--paired` variants | None; dense and reranked variants run local ONNX models |
| `tools/qualify_longmemeval.py` | The frozen LongMemEval-S screen (`freeze`, `run`, `packets`, `score`), and the coverage scorer `retrieval_recall.py` and `amb/recall.py` use | Its `run` phase calls the configured answering model |
| `tools/amb/graphicle_provider.py` | A `graphicle` provider for an unmodified [Agent Memory Benchmark](https://github.com/vectorize-io/agent-memory-benchmark) (AMB) install, whose loaders, prompts, models and judging stay AMB's | AMB's reader and judge |
| `tools/amb/recall.py` | Retrieval recall on the documents AMB's loaders produce, per variant and AMB category | None |
| `tools/amb/qa.py` | AMB's answer and judge steps over the saved packets of an `amb/recall.py` run | AMB providers or local MLX models |
| `tools/expand_queries.py` | Model-written query variants, written ahead of time for `retrieval_recall.py --query-variants` (the `mq-` variants); `query_expansion_prompt.txt` is its prompt | Any OpenAI-compatible server |
| `tools/azure_memory_eval.py` | An auditable Azure OpenAI batch transport with content-addressed receipts, for answer evaluations | Azure OpenAI |

## What a run needs

- A `graphicle` binary. The tools default to the runtime crate's builds
  (`crates/graphicle/target/debug/graphicle` for `retrieval_recall.py`, the
  release build for `amb/recall.py`); pass `--binary`, or set
  `GRAPHICLE_BIN` for the AMB provider and the tests. Dense and reranked
  variants need a build with `--features local-inference` and local ONNX
  model folders (`--embedder`, `--reranker`).
- The datasets, which are not shipped. `retrieval_recall.py` reads the
  cleaned LongMemEval release from `--dataset` (`qualify_longmemeval.py` pins
  its SHA-256). AMB is imported from a checkout at run time (`--amb`); its
  repository had no licence at the pinned commit, so nothing is copied from
  it.
- For model-calling tools, credentials in the process environment, never on
  the command line: `azure_memory_eval.py` reads `AZURE_OPENAI_BASE_URL` and
  `AZURE_OPENAI_API_KEY1`.

Write run outputs under `runs/` (ignored here) or outside the repository.
The recall runners freeze a protocol with the SHA-256 of the binary, the
dataset and their own sources before any outcome is recorded; the answer
tools record the hashes of their prompts, provider and transport.

```sh
python3 tools/retrieval_recall.py --binary /abs/graphicle \
  --dataset /abs/longmemeval_s_cleaned.json --variants l12-lexfq,lex-rrf \
  --paired l12-lexfq,lex-rrf --out runs/lme-s
```

## Variants

`retrieval_recall.py` names its requests (`VARIANTS`): budgets are explicit,
and options a binary may default are sent as off unless a variant sets them
(`RECIPE_OFF`), so a name means the same request on every binary. A current
binary refuses retrieval options it does not support.

## Tests

```sh
GRAPHICLE_BIN=/abs/graphicle python3 -m pytest tests
```

Tests that need the binary are skipped without one. The AMB tests run without
AMB, on synthetic stand-ins for its loaders.

## Not here yet

The answer-level harness of the October 2026 evaluations (case builders, the
`observer.context` renderer, paired statistics, each run's protocol and
results) and the BEAM recall runner are still research tools; they join this
kit with the published evaluation.
