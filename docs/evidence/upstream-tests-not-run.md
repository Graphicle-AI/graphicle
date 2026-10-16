# Upstream tests the parity checks do not run

The comparison checks of the graph engine run most of the test files
(`tests/test_*.py`) of Graphify 0.9.88 (commit `a9860f70`) against the engine.
A file counts as run when at least one of its tests runs and passes in a check,
or when the engine fails one of its tests and the check's receipt records that
as a known difference. This page lists the others, with the reason each one
is not run, so that the parity claim in
[benchmarks.md](../benchmarks.md#parity-with-graphify) can be read against
it. The known differences are in
[upstream-tests-known-differences.md](upstream-tests-known-differences.md), and
every file's status is in
[upstream-tests-0.9.88.json](upstream-tests-0.9.88.json).

- **Not applicable.** The file tests something the engine does not
  contain, or something that exists only in Python or in Graphify's own test
  setup.
- **Not yet run.** The file tests behaviour the engine ports, and no check
  runs it yet. Other checks and the engine's own tests may cover some of that
  behaviour; this page does not claim that they do.

Against Graphify 0.9.77, from which the port was made, most of these files
are unchanged; the tables mark the new and changed ones. "Covers" names the Graphify modules a file
imports. The list was made on 14 October 2026 from the run of the widened
checks on that day, which recorded every upstream test's outcome, and from its
receipts.

## Not applicable

| Test file | Since 0.9.77 | Why it is not run |
|---|---|---|
| `tests/test_atomic_canvas_export.py` | unchanged | It observes atomic writes by patching Python's `os.replace`, which the engine's writes do not call |
| `tests/test_backend_env_isolation.py` | unchanged | Graphify's own test fixture that clears the backend variables (it reads the Python source of `detect_backend`) |
| `tests/test_backend_extras.py` | changed | Python packaging: the pip extra of the claude backend and its install hint |
| `tests/test_cloud_cta.py` | new | Graphify Cloud's call to action, which the port leaves out |
| `tests/test_extractors_registry.py` | unchanged | Python module layout: `graphify.extract` re-exports the same function objects |
| `tests/test_home_sandbox.py` | unchanged | Graphify's own test fixture that redirects HOME |
| `tests/test_js_walk_order.py` | new | A speed-up inside a private Python function (the reference's `_walk_js_tree` cursor walk, not ported); no extraction runs |
| `tests/test_language_resolvers.py` | unchanged | It registers Python functions as resolvers; the engine's registry runs native passes, which `check_resolver_registry` drives with equivalent recorded scenarios |
| `tests/test_manifest_tomli_required.py` | unchanged | Python before 3.11 without tomli |
| `tests/test_multigraph_compat.py` | unchanged | It probes the Python runtime and NetworkX; `check_multigraph` compares the engine's own graph backend probe |
| `tests/test_pdf_extra_warning.py` | unchanged | It hides the optional pypdf package to test the missing-extra warning; the engine's PDF reader is built in, so the warning cannot occur |
| `tests/test_pin_hash_seed.py` | unchanged | Python's PYTHONHASHSEED, which the interpreter reads when it starts |
| `tests/test_python_parse_memoization.py` | unchanged | A speed-up inside private Python functions |
| `tests/test_serve_http.py` | changed | It skips unless the `mcp` and `starlette` packages are installed, and it drives the reference's HTTP app inside Python; `check_native_workflows` drives the engine's HTTP transport |
| `tests/test_skill_codex_semantic_merge.py` | new | The text of Graphify's own skill; the engine installs its own skill files |
| `tests/test_skill_semantic_all_cached.py` | unchanged | The text of Graphify's own skill; the engine installs its own skill files |
| `tests/test_skillgen.py` | changed | Graphify's skill generator (`tools/skillgen`) and the skill files it writes; the engine ships its own |
| `tests/test_skillgen_input_path_injection.py` | changed | Graphify's skill generator (`tools/skillgen`) and the skill files it writes; the engine ships its own |
| `tests/test_walk_python_tree_iterative.py` | unchanged | A speed-up inside private Python functions |
| `tests/test_wheel_packaging.py` | unchanged | Python packaging: it builds the wheel |

## Not yet run

| Test file | Since 0.9.77 | Covers | Note |
|---|---|---|---|
| `tests/test_anthropic_custom_endpoint.py` | unchanged | `llm` |  |
| `tests/test_antigravity_install.py` | unchanged | command line |  |
| `tests/test_atomic_version_stamp.py` | unchanged | `install` |  |
| `tests/test_claude_md.py` | unchanged | command line |  |
| `tests/test_cli_broken_pipe.py` | unchanged | command line, as a subprocess | Starts the command line with `subprocess.Popen`, which the harness does not route to the engine |
| `tests/test_falkordb_integration.py` | unchanged | `build`, `export` |  |
| `tests/test_file_truncation_warning.py` | unchanged | `llm` |  |
| `tests/test_graphdb_push_indexes.py` | unchanged | `exporters.graphdb` |  |
| `tests/test_hook_chain_survives_skip.py` | unchanged | `hooks` |  |
| `tests/test_id_normalization_contract.py` | unchanged | `build`, `extract`, `ids`, `mcp_ingest`, `symbol_resolution` | Skips as a whole: it needs `hypothesis`, which the pinned reference environment does not install |
| `tests/test_install_references.py` | changed | command line | The one test `check_install` selects from it always skips (every host bundle has shipped) |
| `tests/test_install_strings.py` | unchanged | command line |  |
| `tests/test_install_upgrade.py` | unchanged | command line |  |
| `tests/test_label_retry.py` | unchanged | `llm` |  |
| `tests/test_labeling.py` | unchanged | command line, `llm`, `watch` |  |
| `tests/test_llm_null_token_counts.py` | new | `llm` | All 4 tests drive mocked Python provider SDKs, so `check_llm` deselects them; `check_llm_execution` compares the same contract on the engine's own transports |
| `tests/test_merge_chunks_validation.py` | changed | command line, `semantic_cleanup` |  |
| `tests/test_ollama.py` | unchanged | `llm` | All its tests drive mocked Python provider SDKs, so `check_llm` deselects them |
| `tests/test_ollama_retry_cap.py` | changed | `llm` |  |
| `tests/test_openai_custom_endpoint.py` | unchanged | `llm` |  |
| `tests/test_oversized_document_slicing.py` | unchanged | `detect`, `file_slice`, `llm` |  |
| `tests/test_pdf_token_estimate.py` | unchanged | `llm` |  |
| `tests/test_pr10_watch_overrides.py` | new | `watch` (through `tests/test_language_override_lifecycle.py`) | Skips as a whole: it needs `watchdog`, which the pinned reference environment does not install |
| `tests/test_provider_registry.py` | unchanged | `llm` |  |
| `tests/test_query_cli.py` | unchanged | command line |  |
| `tests/test_settings_merge.py` | unchanged | `install` |  |
| `tests/test_skill_version_warning.py` | unchanged | command line |  |
| `tests/test_uninstall_scope.py` | changed | `install` |  |
