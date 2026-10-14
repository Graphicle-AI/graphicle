# Third-party attribution for the Graphicle memory library

Copyright (c) 2026 KarLex AI, Inc. The memory library is licensed under the Apache License, Version 2.0 (`LICENSE`). Adapted third-party portions keep their upstream MIT or Apache-2.0 terms; the notices of the Apache-2.0 sources are reproduced in the repository's `NOTICE` and their licence texts are in `licenses/`. This is not a relicensing of any upstream repository, nor a claim of original authorship of its algorithms. Enterprise-licensed Onyx code is excluded.

Selected implementations and designs were translated or adapted from the pinned sources below. This is not a port of each entire application. Modified portions are identified in UPSTREAM.json. Upstream MIT and Apache-2.0 terms continue to apply to their adapted portions.

## Graphify-Labs/graphify

https://github.com/Graphify-Labs/graphify/tree/20a20d30d8e7eef77675651f0199d87f913bd3e7

49 pinned syntax grammar selections, declarations, imports, calls and original source coordinates; expanded generic language analyses remain incomplete and do not claim compiler type resolution.

License and notices: licenses/Graphify-Labs__graphify__LICENSE.txt, licenses/Graphify-Labs__graphify__LICENSE-MIT.txt, licenses/Graphify-Labs__graphify__NOTICE.txt

## MemTensor/MemOS

https://github.com/MemTensor/MemOS/tree/a7367d07e55db61099f7b4e2c1108bc5831a24f3

Scoped exact-reuse cache and prioritized projection readiness adaptations. The Rust cache stores lexical features and source layouts, not embeddings; no Redis queue or scheduler service port.

License and notices: licenses/MemTensor__MemOS__LICENSE.txt

## getzep/graphiti

https://github.com/getzep/graphiti/tree/16cdf7045378c8d53ae01f94e2fa60d238cb0f68

RRF, explicit-clock contradiction intervals and contextual semantic task contract. Candidate ties are deterministic by ID; unknown clocks remain unknown.

The alias ladder of `src/graph/canonical.rs` (exact normalised names, then a Shannon-entropy gate, then MinHash Jaccard of character 3-gram shingles of at least 0.9) follows `graphiti_core/utils/maintenance/dedup_helpers.py` at the same commit.

Copyright 2024, Zep Software, Inc. (the copyright notice of the adapted Graphiti files, retained in `src/retrieval.rs`, `src/temporal.rs`, `src/semantic.rs` and `src/graph/canonical.rs`).

License and notices: licenses/getzep__graphiti__LICENSE.txt

## mem0ai/mem0

https://github.com/mem0ai/mem0/tree/a39a802bbc93e85b820078cd3c4dbaf53af25dbe

Contextual, attributed fact extraction and update task contract; connected model supplies interpretations. Instructions to conceal actual provenance are not retained.

Later revision https://github.com/mem0ai/mem0/tree/94c3fe9f238f3dbf29c9ce98643bd71eb13077cd: rule-based entity candidates (`mem0/utils/entity_extraction.py`) adapted in `src/entities.rs` without spaCy, to name the question terms no source mentions.

License and notices: licenses/mem0ai__mem0__LICENSE.txt

## microsoft/graphrag

https://github.com/microsoft/graphrag/tree/82b87bf0434fc69f857663280e2c36a1c48e52fe

Distinct-source community weights and token-bounded context for supplied reviewed reports; no automatic community report synthesis.

License and notices: licenses/microsoft__graphrag__LICENSE.txt

## onyx-dot-app/onyx

https://github.com/onyx-dot-app/onyx/tree/4d0a9c9ea3bd69d4ac4a3de03db93d27dec90307

Token-bounded chunk accumulation adapted to preserve exact UTF-8 original source bytes. Enterprise code and connector catalogue excluded.

Later revision https://github.com/onyx-dot-app/onyx/tree/b2186a2dba8123ab597e3d28043b6e64619bf135: multi-query fusion. Case-insensitive query deduplication that adds the weights of repeats (`deduplicate_queries` in `backend/onyx/tools/tool_implementations/search/search_tool.py`) and one weighted reciprocal-rank-fusion ranking per query (`weighted_reciprocal_rank_fusion` in `search_utils.py` of that directory), adapted in `src/retrieval.rs` (opt-in `query_variants`; each variant fuses as a weighted `lexical_q{i}` arm, and a caller's dense ranking for it as `semantic_q{i}`). The variant kinds follow RAG-Fusion (plain rephrasings), HyDE (Gao et al., 2022) and query2doc (Wang et al., 2023) for hypothetical answer passages; implemented independently from the papers.

License and notices: licenses/onyx-dot-app__onyx__LICENSE.txt

## vectorize-io/hindsight

https://github.com/vectorize-io/hindsight/tree/680406b3dd9cca2108c7f0e204820a09b4e30906

Version-2 opaque document blocks, eight delta operations, missing-grounding detection and observation contract. Input bounds, stricter fences, transitive invalidation and approval-separated application added.

Later revision https://github.com/vectorize-io/hindsight/tree/eb021da3b2501911e4b57c82b3de1123572a200e: English period rules from `hindsight_api/engine/temporal_periods.py` adapted in `src/temporal.rs` (`temporal.window`).

Later revision https://github.com/vectorize-io/hindsight/tree/eb021da3b2501911e4b57c82b3de1123572a200e: `resolve_section_anchor` (an `add_section` anchor may name a section added earlier in the batch, a slug or a heading) and the wrong-section block report of `resolve_block`, from `hindsight_api/engine/reflect/delta_ops.py`, adapted in `src/document.rs` (`document.delta`).

License and notices: licenses/vectorize-io__hindsight__LICENSE.txt

## Retrieval designs read 2026-09-30

These are independent implementations of published designs; no upstream code was copied. `design_adaptations` in UPSTREAM.json records the inspected files and their hashes.

- microsoft/Memora, https://github.com/microsoft/Memora/tree/dec3f8f2444eace7004fc084abe1be9f3d88270e: a weight per ranking in reciprocal-rank fusion (`arm_weights`, `rrf_k` in `src/retrieval.rs`), after `_merge_results_with_rrf` in `src/memora/core/memory.py`. License: licenses/microsoft__Memora__LICENSE.txt
- openmemind/memind, https://github.com/openmemind/memind/tree/7536460664228a7b48e41c4ed61f2a75af244b17: weighted fusion and the strong-signal rule of `evidence_strength`, after `ResultMerger.java` and `ScoringConfig.java`. License: licenses/openmemind__memind__LICENSE.txt
- HKUDS/LightRAG, https://github.com/HKUDS/LightRAG/tree/453dce83d6d0354a06e46c8d4029a0895c4e054b: per-origin quotas falling linearly from `max` to `min` with leftover polling (`origin_quota`), after `lightrag/operate.py` and `lightrag/utils.py`. License: licenses/HKUDS__LightRAG__LICENSE.txt
- FreedomIntelligence/Tiermem, https://github.com/FreedomIntelligence/Tiermem/tree/dba57bf2ce91a9920fdebaf5ffb0e6fc029fc398: weak-evidence and aggregate-question features of the `evidence_strength` report, after `src/linked_view/router.py`. License: licenses/FreedomIntelligence__Tiermem__LICENSE.txt
- xiaowu0162/LongMemEval, https://github.com/xiaowu0162/LongMemEval/tree/9e0b455f4ef0e2ab8f2e582289761153549043fc: a further ranking over the user's own turns (opt-in `speaker` in `src/retrieval.rs`; the `"auto"` choice of the assistant's or the user's side from the question is Graphicle's own), after the user-only retrieval keys of `process_item_flat_index` in `src/retrieval/run_retrieval.py`; the opt-in `key_expansion: "merge"`, which adds a row's `abstractions` to its lexical text, after the key = value + fact join of `resolve_expansion` in `src/retrieval/index_expansion_utils.py`. License: licenses/xiaowu0162__LongMemEval__LICENSE.txt
- SmartSearch (arXiv 2603.15599; no code published): term-focused window trimming (`pack_focus` in `src/packing.rs`), from the paper's description. The paper is licensed CC BY 4.0 (Derehag, Calva and Ghiurau).
- M. F. Porter, "An algorithm for suffix stripping", Program 14(3), 1980, and the reference implementation's two documented departures, https://tartarus.org/martin/PorterStemmer/: English suffix stemming of BM25 terms (opt-in `stem`, `src/stem.rs`), as in Lucene's and Anserini's English BM25 baselines.

## Rust dependencies

Cargo.lock pins the linked and build-time dependencies. The licence texts of the crates linked into a release binary are in the `THIRD_PARTY_LICENSES.html` of the program that ships this library (`../graphicle-graph/licenses/THIRD_PARTY_LICENSES.html` for the graph engine, `../graphicle/THIRD_PARTY_LICENSES.html` for the `graphicle` program), generated from Cargo.lock by `cargo about`. Grammar-specific licenses and pinned revisions are in GRAMMAR_NOTICES.json, and each grammar project's licence text is in licenses/grammars/. Licence texts recovered from the source repositories of crates whose packages carry none are in licenses/upstream-crates/ (licenses/CRATE_SOURCE_NOTICES.json). Declared-license fallbacks are identified in licenses/DECLARED_CRATE_NOTICES.json; they are not represented as recovered upstream copyright notices.
