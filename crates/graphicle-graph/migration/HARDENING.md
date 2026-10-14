# Rust hardening and port completion

This report records the initial hardening phase and its qualified release.
The subsequent optimization pass and its current verification are recorded in
OPTIMIZATION.md. The measurements below remain historical
results for the exact binaries named here.

The pinned Graphify application has native implementations for all 87 Python
application modules, including the final extraction, CLI, watcher, hooks,
installer, MCP and package-export workflows. The qualification status and exact
module evidence are in [STATUS.md](STATUS.md) and [AUDIT.json](AUDIT.json).
The 294 original test files remain Python development references.

## Native interfaces and invariants

* `Error` and `ErrorKind` replace string-only results at the Graphify and memory
  core boundaries. Concrete I/O, JSON, parser and model/provider errors can
  retain their causes. Compatibility error text remains on the JSON wire;
  legacy validation messages still exist within typed error variants.
* Native `NodeId`, `SourceSpan`, `NodeRecord`, `EdgeRecord` and `GraphDocument`
  validate identifiers, ordered source ranges, reserved metadata fields,
  duplicate nodes and edge endpoints. Record conversion consumes attributes.
  The NetworkX compatibility adapter remains flexible: it supports numeric and
  tuple identifiers and extensible JSON metadata. This is not a claim that every
  extractor's metadata is now a closed Rust enum.
* Graph indices and mutation-sensitive storage are private outside the crate.
  Invalid edge IDs/keys are rejected before endpoints are inserted. Removal
  preserves ordering, adjacency and parallel edges in all four graph modes.
* Public operations use borrowed graph references and named options/context
  structures in place of the high-arity argument lists identified by Clippy.
  All 17 original package exports are available as native public functions.

## Allocation and failure handling

`rust-memory::dispatch` consumes a forwarded request and changes its operation
name without cloning the complete JSON payload. Graph lookups hash borrowed
identifiers; numeric/Python identifier equivalences remain tested. Borrowed edge
and neighbor iteration avoids temporary traversal vectors. Node removal moves
surviving adjacency instead of cloning all survivors.

RRF accumulation borrows identifiers until returning results and has stable tie
ordering. Evidence packing and community context append once and roll back a
rejected candidate, avoiding a copy of the entire accumulated context per trial.
Unchanged generated wiki/Obsidian files retain their timestamps and bytes.
These are targeted allocation reductions; counting every remaining `.clone()`
would not distinguish necessary ownership from avoidable work.

Graphify dispatch and memory-core dispatch catch Rust unwinding panics. The
CLI, MCP and embedding boundaries validate input shape and bound JSON frames
and output serialization. Oversized stdio input is drained before processing
the next request; a closed output pipe ends normally. HTTP preserves distinct
bad-input, size-limit, provider and internal-error statuses. The C ABI keeps its
version, size caps and caller ownership contract.

This does not assert that every internal helper is panic-free: checked constant
initializers and internal invariants still use assertions/expectations, and
`catch_unwind` cannot recover from process aborts or allocation failure. New
native tests exercise malformed inputs and recoverable failure paths directly.

## Verification

The repeatable gate is `tools/verify_rust.py`: formatting, strict
`cargo clippy --all-targets -- -D warnings`, and native tests across Graphify,
the memory core and runtime service. No blanket Clippy suppression was added.
The Common Lisp first-match loop, remaining lint failures, named argument
contexts and current Rust lifetime diagnostics were corrected. The full oracle
run also caught and corrected a Kotlin diagnostic regression for missing
anonymous parser tokens; a native regression now checks its exact line number.
Vendored HCL and SQL C scanners still emit their existing C compiler warnings;
Rust Clippy passes.

Native regression coverage includes graph identifier equivalence and mutation,
typed model validation, error causes, JSON/FFI limits, broken pipes, MCP recovery,
cache invalidation, partial-label retries, export ownership and timestamps,
installer argument/heading handling, RRF/MMR, temporal intervals, dependency
cycles, revision scheduling, distinct-source community evidence and UTF-8
packing offsets. Tests assert behavior and failure contracts independently of
the Python oracle.

The final comparison suite uses `tools/qualification.py` and
`tools/qualify_all.py`. The build records every Rust source, asset, lockfile and
vendored dependency hash before and after Cargo runs, then snapshots the binary.
Each receipt checks that source set and records the comparison-tool hashes.
`record_qualification.py` requires the full suite and current receipts for every
application module. `report_migration.py` detects stale sources, binaries'
recorded identities, manifests and comparison tools.

Original fixtures and independent differential cases cover the native modules.
Real temporary-project workflows cover cold/warm extraction, modifications,
deletions, failed-file retries, conservative replacement, lock contention,
watcher events, shared output roots, semantic cache recovery, labeling and
missing-only labeling, provider configuration, local exports, merge operations,
installer round trips, MCP stdio and HTTP sessions/authentication/reload. Model
calls use explicit recorded transports or loopback providers; this does not
claim live production-service or model-quality qualification.

The full optimized-release comparison passed **86/86 checks**, covering all
**87 application modules / 70,914 source lines**, with zero stale receipts.
The run report records every
command and result. The release binary SHA-256 is
`6df6bbd02c1ab1058535bd37a8a7aae406f66554600c753ff399a613b55c4e01`;
its manifest
binds the source set and build.

The final native gate passed with
unchanged sources throughout the run, using Rust 1.95.0:

| Crate | Native tests passed | Formatting | Strict Clippy |
| --- | ---: | --- | --- |
| `graphify-rs` | 68 | Pass | Pass |
| `rust-memory` | 18 | Pass | Pass |
| `runtime-service` | 96 | Pass | Pass |

Migration artifacts follow the repository’s existing local-only ignore rule;
the Rust source and reusable verification tools are separate.

## Measured performance

The benchmark compares persistent Rust and Python workers receiving identical
JSONL requests. It checks equivalent results for every sample, warms each case
once, alternates worker order, disables extraction caching, and records raw
samples plus medians. Workloads cover parsing/indexing/reverse traversal of
2,000- and 20,000-node graphs, and extraction of 100 Python files. The earlier
release binary was frozen before the allocation/typing changes; the final
release binary is bound to the final source manifest.

Measurements on this macOS ARM64 host used **15 timed samples per workload**
after warmup, with compilation and qualification finished. Each cell below is a
median in milliseconds; the Python column comes from the final release run.

| Workload | Earlier Rust | Hardened Rust | Python | Python / hardened Rust |
| --- | ---: | ---: | ---: | ---: |
| Load and traverse 2,000-node graph | 12.30 | 11.32 | 16.14 | 1.43× |
| Load and traverse 20,000-node graph | 145.54 | 125.46 | 203.52 | 1.62× |
| Extract 100 Python files, cache disabled | 117.11 | 113.06 | 133.87 | 1.18× |

The larger graph's native median decreased by **13.8%**. The smaller graph and
extraction medians decreased by 7.9% and 3.5%, respectively, but sample spread
and the changing Python timings make small differences unsuitable as strong
optimization claims. These are observations from two sequential runs, not a
controlled attribution of each code change. The observed final Rust/Python
ratios were 1.18–1.62×.

Both earlier-release and
hardened-release receipts retain every raw
timing, result hash, tool hash, binary identity and host/runtime details. All
samples returned equivalent results within their run. Temporary extraction
roots differ between runs, so extraction-result hashes are not expected to
match across those runs.

These local measurements cover those workloads and this machine. They do not
establish universal speedups, model accuracy, peak-memory bounds, or performance
of unmeasured external services. Source-level allocation reductions are reported
separately from measured elapsed time.

## Upstream fixes and remaining scope

[The upstream review](upstream-review/REVIEW.md) records seven adopted PR fixes,
an open cache-invalidation issue, and additional bugs found by native tests.
It distinguishes metadata screening from source review and identifies every
post-pin oracle adaptation. The pinned Swift grammar limitation remains
reproducible. The new languages and all other 0.9.66 features are not implicitly
included in this 0.9.65 port.

The native library is callable through the memory core. Completing this port
does not automatically add every graph/reflection/lifecycle algorithm to the
service's product workflows or implement every workstream of the separate memory
product plan. External integrations and optional build features retain their
explicit configuration requirements. Validation was performed on the current
macOS ARM64 host; it does not establish a Windows or Linux release qualification.
