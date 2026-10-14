# Graphicle graph engine: third-party notices

Copyright (c) 2026 KarLex AI, Inc. The graph engine is licensed under the
Apache License, Version 2.0 (`LICENSE`); `NOTICE` carries the notices that
Apache-2.0 section 4(d) requires. This file describes each adapted source and
where its licence text is kept.

This package ports Graphify 0.9.88, commit
`a9860f70dc3cc2e9c2d8cb923484cfa0592b9067`.
Original authors and notices are retained in `licenses/`.
Adapted implementation files identify the corresponding upstream file.

The original application code is translated into Rust. The migration ledger
distinguishes runtime ports, tests, unchanged fixtures, assets and build tooling.
Original Graphify files remain in the pinned source archive and extracted tree.
No completeness or performance claim follows from copying an interface.

Unicode case-folding, word-character and identifier tables are generated from
CPython's Unicode 15.0.0 database for compatibility with the pinned Python 3.12 reference runtime.
NFKC normalization uses the separately licensed `unicode-normalization` crate.
The Unicode data license is retained in `licenses/UNICODE.txt`.

The Markdown port's YAML-1.1 scalar grammars follow PyYAML; its MIT notice is
retained in `licenses/PyYAML-LICENSE`. YAML parsing itself uses `yaml-rust2`.

HTML5 character-reference data and unescape rules are adapted from CPython
3.12 into native Rust; the PSF license and copyright notice are retained in
`licenses/PYTHON-LICENSE.txt`. No Python interpreter is linked or invoked.

Summary of changes to CPython 3.12 (PSF License Version 2, section 3). Each
part below is translated into Rust, limited to the behaviour the pinned Python
reference relies on, and has no Python runtime: `html.unescape` and its HTML5
entity table (`src/html_compat.rs`, `src/html_entities.json`);
`datetime.fromisoformat` (`src/python_datetime.rs`); `textwrap.shorten` and
`TextWrapper._split` (`src/extract_languages.rs`); the `str` hash (SipHash-1-3)
and the iteration order of string sets (`src/python_compat.rs`);
`random.Random` seeding, `shuffle` and `sample` (`src/cluster.rs`); the
Unicode 15.0.0 case-folding, word-character and printable tables
(`src/unicode15.rs`, `src/python_repr_data.rs`, `src/regex_compat.rs`); and the
`ipaddress` reserved-network tables (`src/ip_policy_data.rs`).

The seeded generators reproduce the reference MT19937 algorithm of Makoto
Matsumoto and Takuji Nishimura, as CPython's `random` module and NumPy's
`RandomState` seed it (`src/cluster.rs`, `src/minhash.rs`); its BSD-3-Clause
notice is in `licenses/MT19937-LICENSE.txt`. The string hash follows the
SipHash implementation in CPython's `Python/pyhash.c` by Marek Majkowski (MIT);
its notice is in `licenses/SipHash-csiphash-LICENSE.txt`.

The Terraform parser retains the exact generated C grammar from the pinned
`tree-sitter-hcl` 1.2.0 source distribution, with a native Rust binding. Its
Apache-2.0 license and checksummed provenance are in `vendor/tree-sitter-hcl/`.
This is a parser dependency, not an additional Graphify application port.

The SQL extractor similarly retains the exact generated C grammar from
`tree-sitter-sql` 0.3.11, with a native Rust binding. Its MIT license and
checksummed provenance are in `vendor/tree-sitter-sql/`.

The R, Solidity, VB.NET and Erlang extractors likewise retain the exact
generated C grammars behind Graphify's optional Python parsers, each with a
native Rust binding: `tree-sitter-vb-dotnet` 0.3.0 (MIT), `tree-sitter-solidity`
1.2.13 (MIT), and the R (`r-lib/tree-sitter-r`, MIT) and Erlang
(`WhatsApp/tree-sitter-erlang`, Apache-2.0) parsers bundled in
`tree-sitter-language-pack` 0.11.0. Licenses and checksummed provenance are in
`vendor/tree-sitter-{vb-dotnet,solidity,r,erlang}/`; `tools/vendor_extractor_grammars.py`
reproduces them from the pinned archives.

The Python parser retains the exact generated C grammar of `tree-sitter-python`
0.25.0 (MIT), the version behind Graphify's pinned Python parser, with a native
Rust binding. Its external scanner carries one fix from `tree-sitter-language-pack`
1.20.0 (MIT): the state serializer no longer writes one byte past its 1,024-byte
buffer on a deeply indented file. The license, the patch and checksummed
provenance are in `vendor/tree-sitter-python/`; the pack's MIT licence, which
covers the patch (Copyright 2025-2026 Kreuzberg, Inc.), is in
`licenses/vendor/tree-sitter-python__patch__tree-sitter-language-pack-1.20.0__LICENSE.txt`.

The Kotlin and PHP parsers likewise retain the exact generated C grammars of
`tree-sitter-kotlin` 1.1.0 (MIT, Amaan Qureshi; its public ABI headers come from
the MIT-licensed `tree-sitter-java` 0.23.5) and `tree-sitter-php` 0.24.1 (MIT),
each with a native Rust binding; licenses and checksummed provenance are in
`vendor/tree-sitter-{kotlin,php}/`. `tools/vendor_engine_grammars.py` reproduces
them and Groovy from the pinned archives.

The Groovy parser retains the generated C grammar of `tree-sitter-groovy` 0.1.2
(tag v0.1.2 of amaanq/tree-sitter-groovy). Its licence basis is a declaration:
the upstream Cargo.toml, retained as
`vendor/tree-sitter-groovy/UPSTREAM-LICENSE-DECLARATION.toml`, names the author
Amaan Qureshi and `license = "MIT"`, but neither the tag nor its Python source
distribution has a licence file or a copyright notice. We rely on that
declaration and ship the standard MIT text with it, without inventing a
copyright line (`licenses/vendor/tree-sitter-groovy__DECLARED-MIT.txt`). On
2026-10-13, v0.1.2 was still the latest tag and the default branch still had no
licence file; a later release with one replaces this basis if its parser is
identical.

Each vendored grammar crate wraps the upstream generated sources with a small
first-party build glue: its `Cargo.toml`, `build.rs` and `src/lib.rs`. That glue
is Copyright (c) 2026 KarLex AI, Inc. and is licensed under the same terms as
the grammar it wraps, so each crate's `license` field covers the whole crate.
Copies of every vendored licence text are also in `licenses/vendor/`, so that
release archives carry them.

Community detection links the existing MIT-licensed `network_partitions` Rust
kernel under `vendor/network_partitions/`. Its algorithms
match the kernel shipped in `graspologic-native` 1.2.5 (Copyright (c) Microsoft
Corporation), which took the MIT-licensed CWTS `networkanalysis` Java
implementation as its starting point; the CWTS notice is in
`licenses/vendor/network_partitions__CWTS-networkanalysis-LICENSE.txt`. The
sources are formatted with rustfmt (seven files differ from the recorded
upstream hashes only in formatting), and three explicit borrowed lifetime
annotations were added in two files for current Rust diagnostics;
`PROVENANCE.json` retains the upstream hashes and records the two annotated
files.
The Louvain fallback follows NetworkX 3.7 (BSD-3-Clause),
and its seeded shuffle follows CPython 3.12 (PSF). Dependency licenses are retained.

Pascal uses the MIT-licensed grammar from `wpostma/tree-sitter-pascal`, commit
`a55068ee73ec993e06d81ca4eb53b85452e79c72`, retained under
`vendor/tree-sitter-pascal/`. Graphify's optional Python parser 0.11.0 switched
to AGPL-3.0-only. That fork is a development comparison reference only; it is
not included in this native package. Both the original Graphify traversal and
its regex fallback are translated. Grammar extensions unique to the AGPL fork
are outside this dependency compatibility claim; see the Pascal receipt.

Optional PostgreSQL introspection uses the `pq-sys` Rust bindings and the
PostgreSQL `libpq` client supplied by `pq-src`. Their upstream licenses and
PostgreSQL copyright notice are retained under `licenses/`. The native client
replaces the Python psycopg dependency; it does not embed a Python interpreter.

Document discovery uses the MIT-licensed `zip`, `calamine`, and `pdf-extract`
Rust libraries in place of Python ZIP/Office/PDF readers. `encoding_rs` retains
its Apache-2.0/MIT and WHATWG BSD-3-Clause notices; `md-5` is MIT/Apache-2.0 and
is used only for the original manifest's change-detection digests. Available
dependency notices are retained under `licenses/`; pinned dependency versions
are recorded in `Cargo.lock`. PDF layout whitespace can differ between the
native parser and pypdf, as recorded in the discovery comparison receipt.

Graph database export uses the MIT-licensed `neo4rs` Rust driver (pinned
0.9.0-rc.10, with routing support) and the BSD-3-Clause `redis` Rust client.
The FalkorDB parameter header encoding follows the MIT-licensed FalkorDB
Python SDK 1.7.1; its license is retained. No FalkorDB server implementation
is bundled. neo4rs's distributed Cargo license/author declaration is retained
because its source package and tagged repository contain no standalone license
file. Provider transport qualification and its limits are recorded separately.

SVG layout follows NetworkX 3.7's Fruchterman–Reingold force and energy
formulations. The large-graph optimizer is the BSD-3-Clause `lbfgsb-rs-pure`
0.1.2 Rust implementation; its license is retained. Native SVG primitives
replace Matplotlib; identical font metrics and large-graph optimizer iterates
are not claimed. Unicode digit ranges used by Obsidian are from the same
CPython Unicode 15.0.0 reference database described above.

The native Robot Framework source lexer follows the Apache-2.0-licensed Robot
Framework 7.5 tokenizer, lexer settings and statement classification. Its
built-in language header/setting tables are retained as JSON data. Copyright
2008–2015 Nokia Networks; copyright 2016– Robot Framework Foundation. The
license is retained at `licenses/robotframework-7.5-LICENSE.txt`. The source
runtime uses Rust; the Python package is used only as a development oracle.

Model token estimates use the MIT-licensed `tiktoken-rs` 0.12.0 implementation
and its bundled cl100k vocabulary. Its license is retained under `licenses/`.
Bedrock Converse uses the Apache-2.0 AWS SDK for Rust (`aws-config` 1.12.0 and
`aws-sdk-bedrockruntime` 1.146.0); their distributed licenses are retained.
The opt-in `bedrock` feature, which the default build leaves out, raises the
minimum Rust version to 1.94.1. Other providers use native HTTP, while the claude-cli
backend launches the user's separately installed official CLI. Python SDKs
are development references and are not runtime provider dependencies.

Chinese query tokenization uses MIT-licensed jieba-rs 0.11.0, with the HMM
parameters from the upstream-compatible jieba-py 0.46.12 release. The source
probabilities are data, not Python execution; both licenses are retained under
`licenses/`. The native adapter preserves the reference ASCII boundaries
inside mixed Chinese/code terms. Builds without the `chinese` feature retain
Graphify's deterministic bigram fallback.

Audio transcription (the opt-in `transcription` feature) links whisper-rs
0.16.0 (Unlicense) and whisper.cpp (MIT), using CPU inference, and decodes audio
through the user's separately installed ffmpeg executable. Their licenses are
retained.
Whisper weights are fetched only when transcription needs them, from a pinned
public model revision with size and SHA-256 validation. This replaces the
Python faster-whisper/CTranslate2 backend; identical transcripts, quantization,
speed and model accuracy are not claimed across those inference engines.
The optional URL-audio path invokes the separately installed official yt-dlp
executable; it is not a Rust rewrite of yt-dlp or a bundled Python interpreter.

URL ingestion uses the MIT-licensed html2text 0.15.5 and its native HTML5 parser
in place of optional Python markdownify. The original basic-strip mode remains
available. Rich Markdown layout may differ between these converter libraries;
source URLs, capture metadata, links and content remain observable in the saved
document. The dependency license is retained under licenses/.

Selected post-pin Graphify fixes retain the same upstream notices. Their PR
heads, release comparison, local adaptations and regression evidence are
recorded in `migration/upstream-review/REVIEW.md` and `adaptations.json`; the
adapted files, by upstream file, are listed in `migration/ADAPTED.json`.

Graph adjacency stores single-edge groups inline with `smallvec` (MIT or
Apache-2.0) and uses randomized `foldhash` (Zlib) for internal lookup tables.
The standalone executable uses `mimalloc` and `libmimalloc-sys` (MIT) as its
Rust allocator; library consumers retain their own allocator choice. These
dependency notices are retained under `licenses/`, with versions in Cargo.lock.


Precise JavaScript/TypeScript resolution (the `precise-js` feature, in the
default build; used with `--precise`) links the MIT-licensed oxc crates (`oxc_parser`, `oxc_semantic`, `oxc_ast`,
`oxc_allocator`, `oxc_span` 0.139.0) and `oxc_resolver` 11.23.0 (MIT). The
SCIP importer reads the Apache-2.0 SCIP index format
(`sourcegraph/scip`, `scip.proto`) with its own decoder; no SCIP code is
included. Grammar packs are loaded with `libloading` (ISC) and built from the
same pinned grammar crates as the default build; the pack `manifest.json`
records each crate and version, and each pack is under its grammar crate's
licence. Release archives ship no grammar packs.

The licence texts of the Rust crates linked into a release binary are in
`licenses/THIRD_PARTY_LICENSES.html`, generated from Cargo.lock by
`cargo about` (see CONTRIBUTING.md). Model weights, external executables
(ffmpeg, yt-dlp, the Claude CLI) and graph database servers are not bundled.
