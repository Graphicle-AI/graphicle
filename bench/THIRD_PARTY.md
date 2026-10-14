# Third-party material in the benchmark kit

Everything under `bench/` is Graphicle's own code, Copyright (c) 2026 KarLex AI,
Inc., licensed under the Apache License, Version 2.0 (the repository's `LICENSE`), except the
parts listed here. Datasets, benchmark harnesses and competing tools are never
copied into the repository; the tools fetch or import them at run time.

## Included, with their licences

| Kit file | Source | Licence | Licence text |
|---|---|---|---|
| `bench/retrieval/aider_repomap.py`: `file_mentions`, `ident_mentions`, `ident_filename_matches` | Aider (https://github.com/Aider-AI/aider), aider-chat 0.86.2, `aider/coders/base_coder.py`: `Coder.get_file_mentions`, `get_ident_mentions`, `get_ident_filename_matches`; copied and modified, as the file's docstring states | Apache-2.0; Aider ships no NOTICE file (checked at tag v0.86.2) | identical to the repository's `LICENSE` |
| `bench/memory/tools/query_expansion_prompt.txt`, and the variant weights in `bench/memory/tools/expand_queries.py` | Onyx (https://github.com/onyx-dot-app/onyx) at commit `b2186a2dba8123ab597e3d28043b6e64619bf135`: the semantic-rephrase and keyword-expansion prompts of `backend/onyx/prompts/search_prompts.py`, adapted, and the weights of `backend/onyx/tools/tool_implementations/search/constants.py` | MIT (content outside Onyx's enterprise folders) | `../crates/graphicle-memory/licenses/onyx-dot-app__onyx__LICENSE.txt` |
| `bench/memory/tools/retrieval_recall.py`: the paired McNemar comparison | genome by Northtek (FrostByte Digital LLC), `benchmarks/lme_qa_or.py` at commit `ea76cf924b2382499a158b8c7ad8a815913c962f` | Apache-2.0, with a NOTICE that the repository's `NOTICE` reproduces | `../crates/graphicle/licenses/NORTHTEKDevs__genome__LICENSE.txt`, `../crates/graphicle/licenses/NORTHTEKDevs__genome__NOTICE.txt` |

The prompt file must stay byte-identical to the archived feature's prompt (its
SHA-256 is recorded with every run), so its notice is kept here rather than in
the file.

## Fetched or imported at run time, never included

- **Agent Memory Benchmark (AMB)**, https://github.com/vectorize-io/agent-memory-benchmark,
  commit `f618ed7b1f0eb9cad7b42e876f91a42f0eadb150`: its repository had no licence
  at that commit, so no AMB code, prompt or data is copied; `bench/memory/tools/amb/`
  imports an unmodified AMB checkout.
- **Datasets** (LongMemEval, LoCoMo, LifeBench, BEAM and the others the tools
  name): downloaded by the user from their publishers, under their own terms.
  No dataset, per-question context, answer or verdict is published in this
  repository.
- **Aider** itself, the models and the inference servers the tools call.
