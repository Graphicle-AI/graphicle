# graphicle-memory: the memory library

`graphicle-memory` holds the source-backed memory operations that the
`graphicle` program ([crates/graphicle](../graphicle/README.md)) runs: passage
ingestion, code-structure analysis, retrieval, evidence packing, time and
revisions, and the shared code-graph functions. It is a Rust library with a
bounded JSON interface. It makes no network or model calls: the program that
links it stores the sources, checks authorization and calls any models.

It depends on the graph engine (the `graphicle` library in
`../graphicle-graph`) and on no code outside this repository.

**This is a selective implementation, not a Rust port of whole applications.**
Selected algorithms and designs of the projects in [NOTICE.md](NOTICE.md) were
translated, adapted or reimplemented. [UPSTREAM.json](UPSTREAM.json) records the
pinned commits, the inspected files with their hashes and the module that uses
each one.

## What it does

| Area | Behaviour | Boundary |
|---|---|---|
| Source ingestion (`ingest`, `text`) | Exact UTF-8 passages with hashes, token and character bounds, line and byte coordinates and continuation | Storage, transactions and authorization belong to the program that links the library |
| Code structure (`structure`, `bindings`) | 49 pinned tree-sitter grammars: declarations, imports, calls and their source locations; Markdown links; explicit imported-call binding and lexical shadowing | Generic analyses are incomplete for some languages, and compiler type resolution is not performed |
| Retrieval (`retrieval`, `stem`, `recall`) | BM25 with optional Porter stems, reciprocal rank fusion with the caller's dense results, speaker-scoped ranking, task modes, diversity and explicit evidence needs | Need coverage is lexical; it does not establish semantic completeness or answer correctness |
| Evidence packing (`packing`) | An exact final token bound, source-first allocation, focused windows, original-source continuation and whole-procedure delivery | Counts with cl100k_base; the answering model's tokenizer may differ |
| Time and revisions (`temporal`, `lifecycle`) | Recorded and event clocks, question periods, temporal contradiction previews and historical memory reads | Unknown time stays unknown |
| Documents and refresh (`document`) | Document blocks, eight delta operations, current-source citations and immutable previews | The agent or model proposes the text; a person reviews it |
| Derived state (`cache`) | Transitive stale-dependency detection, source-removal propagation and scoped, bounded caches | |
| Standing statements (`standing`) | A user's standing instructions and preferences, and "I've never" conflicts, found in labelled turns without a model call | |
| Code graph (`graph`) | Shared identity, document anchoring, canonicalization, stable communities, graph retrieval and cross-repository import linking | |

`archived` lists the options and values that this release does not support: a
request that turns one on gets an explicit error instead of being ignored.

## Build and test

The grammar pack compiles its 49 grammars from parser sources in `.tslp-root`,
at the repository root (see `.cargo/config.toml`). Prepare them once
per checkout, then build from this folder:

```sh
python3 ../graphicle/tools/prepare_parser_sources.py    # once per checkout
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
```

Rust 1.94.1 or newer and a C compiler are needed. After the preparation, builds
need no network.

## Interfaces

- **Rust.** The `graphicle` program calls the library directly. Errors are
  typed (`Error`, `ErrorKind`).
- **JSON lines.** `graphicle-memory --jsonl` reads one request per line and
  writes one response per line, with bounded framing shared with the graph
  engine. Operations named `graph.*` are forwarded to the engine.
- **C ABI.** `src/lib.rs` defines ABI version 1. The library returns owned bytes
  that the caller releases with `graphicle_free`, passing back the exact
  pointer and length it received.

## Licence

Apache License 2.0 ([LICENSE](LICENSE)). The notices of the adapted projects
are in [NOTICE.md](NOTICE.md); their licence texts, and those of the grammars
in [GRAMMAR_NOTICES.json](GRAMMAR_NOTICES.json), are in `licenses/`.
