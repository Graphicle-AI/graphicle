# Contributing

This crate, the Graphicle graph engine, is a Rust port of a
pinned Graphify release. Changes to ported behaviour must keep the port equal
to the pinned Python implementation, or record the deliberate difference with
evidence.

## Licence and sign-off

These rules cover every Graphicle Core crate in this repository: the graph
engine (this folder), the memory library (`../graphicle-memory/`), the runtime
and the `graphicle` program (`../graphicle/`) and its SDKs (`../../sdk/`). All of
them are licensed under Apache-2.0, Copyright (c) 2026 KarLex AI, Inc.

Contributions are accepted under the project licence, Apache-2.0 (inbound =
outbound, Apache-2.0 §5). There is no contributor licence agreement. Every
commit must carry a Developer Certificate of Origin 1.1 sign-off
(<https://developercertificate.org>):

```sh
git commit -s   # adds: Signed-off-by: Your Name <you@example.com>
```

The sign-off certifies that you wrote the change or otherwise have the right to
submit it under Apache-2.0. Commits without it are not merged.

## Clean-room rule

Only permissively licensed sources (Apache-2.0, MIT, BSD, ISC, Unlicense, Zlib
and similar) may be read for porting. Never open, copy or translate code from
AGPL, SSPL, BUSL, Elastic, PolyForm or other non-permissive projects: this
includes the Firecrawl server (`apps/api`), GitNexus, context-mode, OpenViking,
maxun, EasySpider, karakeep, SearXNG, khoj and PyMuPDF. Where compatibility with
such a project is wanted, work only from its public documentation and its
permissively licensed SDK type definitions.

## Attribution

- Rust files adapted from Graphify start with an adaptation header naming the
  pinned release and the upstream files. `tools/mark_adapted_files.py` writes
  it from the comparison receipts; `--check` verifies it.
- A new upstream source needs an entry in `NOTICE.md` and its licence text in
  `licenses/` (or the vendored crate's `LICENSE`) before code derived from it
  is merged.
- Vendored grammars keep their exact generated sources, `LICENSE` and a
  checksummed `SOURCE.json`; regenerate them only with their `tools/vendor_*.py`.

## Dependencies

Pin new dependencies (`=x.y.z` or a minimal compatible range) and keep builds
reproducible with `--locked`. Before submitting:

```sh
cargo deny check                                   # licences, advisories, bans, sources
cargo about generate --frozen --fail --all-features about.hbs -o licenses/THIRD_PARTY_LICENSES.html
```

`deny.toml` rejects every licence outside the allow list; the few permissive
exceptions are per crate and documented there. `licenses/THIRD_PARTY_LICENSES.html`
(and `../graphicle/THIRD_PARTY_LICENSES.html`, generated the same way in
`../graphicle`) is committed and ships in the release archives. Regenerate it
whenever `Cargo.lock` changes; it is never edited by hand.

## Verification

Every change must pass formatting, strict Clippy and the native tests, and any
change to ported behaviour must pass the comparison suite against the pinned
Python reference (see README, "Verification and performance"):

```sh
cargo fmt --check
cargo clippy --locked --all-targets --features postgres,bedrock,transcription -- -D warnings
cargo test --locked --all-targets --features postgres,bedrock,transcription
```

The default build leaves out the opt-in `bedrock` and `transcription`
features, so these commands, CI and `tools/verify_rust.py` name them; the
transcription feature needs CMake. `tools/verify_rust.py` also builds the
memory library and the runtime: prepare their parser sources once first, with
`python3 ../graphicle/tools/prepare_parser_sources.py`.

A bug fix to ported code needs a comparison case or native regression test that
fails before the fix. A deliberate deviation from upstream is recorded in
`tools/upstream_fixes.py` and `migration/upstream-review/adaptations.json`.
