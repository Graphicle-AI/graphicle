# Graphicle: notices and licences

Copyright (c) 2026 KarLex AI, Inc. Graphicle Core is licensed under the Apache
License, Version 2.0 (`LICENSE`). `NOTICE` holds the notices that Apache-2.0
section 4(d) requires: those of Graphify and genome, the retained copyright
lines of adapted files, and the vendored third-party code.

Each crate of Graphicle keeps its own long-form notices and licence texts. The
release archives carry copies of them under `licenses/<crate>/`, laid out as
in the crate's folder: the notices, the provenance records they name (`UPSTREAM.json`
and `GRAMMAR_NOTICES.json`) and every licence text, so each licence file a
notice or record names is in the archive too. Source files that a notice names
(vendored code, adapted modules) are in the source repository, not in the
archives.

| Crate | Long-form notices | Licence texts | Rust crate licences |
|---|---|---|---|
| The `graphicle` program (`crates/graphicle/`) | `crates/graphicle/NOTICE`, `crates/graphicle/NOTICE.md`, with `UPSTREAM.json` and `GRAMMAR_NOTICES.json` | `crates/graphicle/licenses/`: adapted sources, the web fonts, the 48 grammar projects linked into the program (`crates/graphicle/licenses/grammars/`) and crates whose packages carry no licence text (`crates/graphicle/licenses/upstream-crates/`) | `crates/graphicle/THIRD_PARTY_LICENSES.html` |
| Graph engine (`crates/graphicle-graph/`, the `graphicle-graph` program) | `crates/graphicle-graph/NOTICE`, `crates/graphicle-graph/NOTICE.md` | `crates/graphicle-graph/licenses/`, with the vendored grammars' texts in `crates/graphicle-graph/licenses/vendor/` | `crates/graphicle-graph/licenses/THIRD_PARTY_LICENSES.html` |
| Memory library (`crates/graphicle-memory/`) | `crates/graphicle-memory/NOTICE.md`, with `UPSTREAM.json` and `GRAMMAR_NOTICES.json` | `crates/graphicle-memory/licenses/`, with the linked grammars' texts in `crates/graphicle-memory/licenses/grammars/` | covered by the two programs' pages |

In the release archives these are `licenses/graphicle/…`,
`licenses/graphicle-graph/…` and `licenses/graphicle-memory/…`.

What the long-form notices cover:

- **The `graphicle` program:** the service's adapted sources (GraphRAG, Mem0,
  Graphiti, Hindsight, MemOS, Onyx, Mastra, claude-mem, OpenMemory,
  agent-memory-server, LongMemEval and others), the grammar pack and the web
  fonts (SIL Open Font License 1.1).
- **Graph engine:** the port of Graphify 0.9.95; the parts derived from CPython
  3.12, with the summary of changes the PSF License asks for; NetworkX, PyYAML,
  Robot Framework, the FalkorDB SDK, jieba, MT19937, SipHash and the Unicode
  data files; the vendored tree-sitter grammars, including the declared-MIT
  basis of the Groovy grammar; and the optional dependencies (PostgreSQL
  libpq, whisper.cpp, the AWS SDK).
- **Memory library:** the adapted designs and code of Graphify, MemOS,
  Graphiti, Mem0, GraphRAG, Onyx, Hindsight and the retrieval designs read for
  comparison.
- **Benchmark kit:** `bench/THIRD_PARTY.md`.

Not bundled: model weights, external executables (ffmpeg, yt-dlp, the Claude
CLI), graph database and cache servers, and ONNX Runtime, which the operator
supplies.
