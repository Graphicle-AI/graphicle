# Contributing to Graphicle

Thank you for helping. This repository holds Graphicle Core: the `graphicle`
program (`crates/graphicle/`), the graph engine (`crates/graphicle-graph/`),
the memory library (`crates/graphicle-memory/`) and the SDKs (`sdk/`). All of
it is licensed under the Apache License, Version 2.0 (`LICENSE`), Copyright (c)
2026 KarLex AI, Inc.

Questions or ideas? Start a [GitHub Discussion](https://github.com/graphicle-ai/graphicle/discussions) or chat with us on [Discord](https://discord.com/invite/rWuU7cWU4E).

## Licence and sign-off

Contributions to every crate and SDK in this repository are accepted under the
project licence, Apache-2.0 (inbound = outbound, Apache-2.0 §5). There is no
contributor licence agreement. Every commit must carry a Developer Certificate
of Origin 1.1 sign-off (<https://developercertificate.org>):

```sh
git commit -s   # adds: Signed-off-by: Your Name <you@example.com>
```

The sign-off certifies that you wrote the change or otherwise have the right to
submit it under Apache-2.0. Commits without it are not merged.

## Third-party code and attribution

- Only permissively licensed sources (Apache-2.0, MIT, BSD, ISC, Unlicense,
  Zlib and similar) may be read for porting or copied. Never copy or translate
  code from AGPL, GPL, SSPL, BUSL or other non-permissive projects.
- A new upstream source needs an entry in the `NOTICE.md` of the crate that
  uses it and its licence text in that crate's `licenses/` folder before code
  derived from it is merged. A file adapted from an Apache-2.0 source keeps the
  upstream copyright notice and states that it was changed.
- `NOTICE` at the repository root reproduces the upstream NOTICE texts that
  Apache-2.0 section 4(d) requires; keep it in step with the crates. The
  root `licenses/` folder is a copy of each part's notices and licence texts
  for the release archives (`NOTICE.md` explains it): change the part's own
  files, and refresh the copy with them.

## Dependencies

Each crate under `crates/` is its own Cargo workspace with its own
`Cargo.lock`. Run `cargo deny check` in each one you change. When a
`Cargo.lock` changes, regenerate the committed licence page of the program it
builds:

```sh
(cd crates/graphicle-graph && cargo about generate --frozen --fail --all-features about.hbs -o licenses/THIRD_PARTY_LICENSES.html)
(cd crates/graphicle && cargo about generate --frozen --fail --all-features about.hbs -o THIRD_PARTY_LICENSES.html)
```

## Crate guides

The graph engine has its own rules for ported behaviour, verification and
vendored grammars in `crates/graphicle-graph/CONTRIBUTING.md`; they apply to
changes there.
