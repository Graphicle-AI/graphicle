# Upstream tests with known differences

On 14 October 2026 the comparison checks of the graph engine were widened: they
now run 354 of the 402 test files of Graphify 0.9.88 against the engine, 75 of
them for the first time. Of the 354 files, 350 pass and 4 have known
differences: 7 tests that the engine fails. Each one is recorded below with the
reason, deselected by its exact name in the check that runs its file, and
listed in that check's receipt. They are not fixed in 0.1.0.

The files the checks do not run are listed in
[upstream-tests-not-run.md](upstream-tests-not-run.md), and every file's status
is in [upstream-tests-0.9.88.json](upstream-tests-0.9.88.json).

## Known differences

| Upstream test | Check | Why the engine fails it |
|---|---|---|
| `tests/test_cli_export.py::test_cluster_only_warns_when_labeling_flags_are_ignored` | `check_cli` | `cluster-only` with saved labels and `--backend` exits 0 without the reference's warning on stderr that the labeling flags are ignored. |
| `tests/test_cli_help.py::test_help_lists_prs_provider_and_export_formats` | `check_cli` | The engine's `--help` text is its own: it does not list the `prs` command, it names the export formats in one list instead of `export <format>` lines, and it lists the `hook-guard` and `hook-check` internals that the reference hides. |
| `tests/test_extract_code_only_cli.py::test_code_only_force_preserves_existing_semantic_layer` | `check_cli` | `extract --force --code-only` keeps the existing semantic layer (the test's checks of the graph pass), but the engine does not print the reference's `existing semantic layer preserved` line. |
| `tests/test_extract_code_only_cli.py::test_code_only_succeeds_without_key` | `check_cli` | `extract --code-only` with no API key builds the code graph, but the engine does not print the reference's `--code-only: skipping` line. |
| `tests/test_extract_code_only_cli.py::test_extract_names_skipped_sensitive_files` | `check_cli` | The engine's `extract` does not print the reference's line that names the files it skipped as potentially sensitive. |
| `tests/test_extract_code_only_cli.py::test_extract_usage_advertises_code_only` | `check_cli` | `extract` with no path prints the engine's error `must specify a path to scan or a --postgres DSN` instead of the reference's usage text, so `--code-only` is not mentioned. |
| `tests/test_typescript_enum_members.py::test_case_only_sibling_members_do_not_duplicate_an_edge` | `check_extract` | Two TypeScript enum members whose names differ only in case (`Value`, `value`) share one node id. The reference keeps one `case_of` edge to it; the engine also emits a second one from the later member's line. |

## Files first run on 14 October 2026

Against Graphify 0.9.77, 2 of these 75 files are new, 2 changed and 71
unchanged. "Left out" names the tests of a file that its check does not run,
with the reason; the known differences above are left out too.

| Test file | Since 0.9.77 | Check | Left out |
|---|---|---|---|
| `tests/test_builtin_global_type_refs.py` | unchanged | `check_extract` |  |
| `tests/test_cargo_missing_manifest.py` | unchanged | `check_cli` |  |
| `tests/test_case_sensitive_resolution.py` | unchanged | `check_extract` |  |
| `tests/test_cli_export.py` | changed | `check_cli` |  |
| `tests/test_cli_help.py` | unchanged | `check_cli` |  |
| `tests/test_community_labels_skill.py` | unchanged | `check_export` | `test_core_fragments_step5_reexport_with_curated_labels`: reads Graphify's own skill fragments; the engine installs its own skill files; `test_skill_step5_reexports_graph_json_with_curated_labels`: reads Graphify's own skill text; the engine installs its own skill files |
| `tests/test_corrupt_graph_json.py` | unchanged | `check_build` |  |
| `tests/test_cpp_method_declarations.py` | unchanged | `check_extract` |  |
| `tests/test_cross_language_call_resolution.py` | unchanged | `check_extract` |  |
| `tests/test_csharp_call_site_generic_args.py` | unchanged | `check_extract` |  |
| `tests/test_csharp_field_generic_args.py` | unchanged | `check_extract` |  |
| `tests/test_csharp_generic_callsites.py` | unchanged | `check_extract` |  |
| `tests/test_csharp_member_nodes.py` | unchanged | `check_extract` |  |
| `tests/test_csharp_tuple_type_refs.py` | unchanged | `check_extract` |  |
| `tests/test_definition_file_portability.py` | unchanged | `check_extract` |  |
| `tests/test_duplicate_annotation_edges.py` | unchanged | `check_extract` |  |
| `tests/test_elixir_import_resolution.py` | unchanged | `check_extract` |  |
| `tests/test_elixir_unqualified_call_scope.py` | unchanged | `check_extract` |  |
| `tests/test_evidence_binding.py` | unchanged | `check_llm` | 12 tests, all but `test_label_identifiers_helper`: they call none of the helpers that `check_llm` compares with the engine, and it runs only the tests that call them |
| `tests/test_export_idempotent_writes.py` | unchanged | `check_export` | `test_to_wiki_rerun_does_not_rewrite_unchanged_articles`, `test_to_wiki_rerun_sweeps_orphaned_relabelled_article`: `check_wiki` compares the engine's pages with the reference's but leaves the reference's files in place, so a rerun check of file times would test the reference; `test_write_text_atomic_if_changed_skips_identical`: checks the reference's `write_text_atomic_if_changed` helper, which no check routes to the engine |
| `tests/test_extract_cache_location.py` | unchanged | `check_extract` |  |
| `tests/test_extract_code_only_cli.py` | unchanged | `check_cli` |  |
| `tests/test_file_node_id_spec.py` | unchanged | `check_extract` |  |
| `tests/test_gemini_hook.py` | unchanged | `check_cli` |  |
| `tests/test_import_self_loops.py` | unchanged | `check_extract` |  |
| `tests/test_incremental.py` | unchanged | `check_extract` |  |
| `tests/test_ingest_url_type.py` | unchanged | `check_ingest` |  |
| `tests/test_inherited_field_receivers.py` | unchanged | `check_extract` |  |
| `tests/test_injection_sentinel_coverage.py` | unchanged | `check_llm` |  |
| `tests/test_issue_3405_python_resolution.py` | unchanged | `check_extract` |  |
| `tests/test_java_enum_members.py` | unchanged | `check_extract` |  |
| `tests/test_js_callback_calls.py` | unchanged | `check_extract` |  |
| `tests/test_js_destructured_export.py` | unchanged | `check_extract` |  |
| `tests/test_js_exported_fn_call_resolution.py` | unchanged | `check_extract` |  |
| `tests/test_js_module_level_anonymous_calls.py` | unchanged | `check_extract` |  |
| `tests/test_js_this_member_expressions.py` | unchanged | `check_extract` |  |
| `tests/test_jsx_component_usage_calls.py` | unchanged | `check_extract` |  |
| `tests/test_kotlin_grammar.py` | unchanged | `check_extract` |  |
| `tests/test_kotlin_object_literal.py` | unchanged | `check_extract` |  |
| `tests/test_long_path_hashing.py` | unchanged | `check_detect` | `test_os_path_adds_prefix_on_win32`: it simulates Windows by patching Python's `sys.platform`; the engine uses the platform it runs on |
| `tests/test_lua_import.py` | unchanged | `check_extract` |  |
| `tests/test_lua_method_calls.py` | unchanged | `check_extract` |  |
| `tests/test_merge_graphs_cli.py` | unchanged | `check_cli` |  |
| `tests/test_no_dedup_flag.py` | unchanged | `check_build` |  |
| `tests/test_node_id_canonical.py` | unchanged | `check_extract` |  |
| `tests/test_non_regular_files.py` | changed | `check_detect` |  |
| `tests/test_non_string_node_ids.py` | unchanged | `check_build` |  |
| `tests/test_objc_field_table_remap.py` | unchanged | `check_extract` |  |
| `tests/test_obsidian_dangling_member.py` | unchanged | `check_export` |  |
| `tests/test_obsidian_filename_cap.py` | unchanged | `check_export` |  |
| `tests/test_out_dir_evidence.py` | unchanged | `check_detect` |  |
| `tests/test_owned_type_sources.py` | unchanged | `check_extract` |  |
| `tests/test_package_json_subpath_imports.py` | unchanged | `check_extract` |  |
| `tests/test_phantom_cross_package_call.py` | unchanged | `check_extract` |  |
| `tests/test_read_hook.py` | unchanged | `check_cli` |  |
| `tests/test_ruby_bare_self_send_calls.py` | unchanged | `check_extract` |  |
| `tests/test_rust_macro_rules.py` | unchanged | `check_rust` |  |
| `tests/test_scala_abstract_methods.py` | unchanged | `check_extract` |  |
| `tests/test_semantic_id_remap_root.py` | unchanged | `check_build` |  |
| `tests/test_skill_part_a_explicit_root.py` | new | `check_extract` | `test_part_a_passes_an_explicit_root`, `test_split_hosts_are_discovered`: reads the skill files that Graphify's skill generator (`tools/skillgen`) renders; the engine ships its own skill files |
| `tests/test_skill_persisted_scan_options.py` | new | `check_detect` |  |
| `tests/test_sourceless_call_targets.py` | unchanged | `check_extract` |  |
| `tests/test_stat_index_multi_root.py` | unchanged | `check_cache` | `test_each_root_gets_its_own_stat_index_file_not_the_first_roots`: it changes the output folder between two calls; `check_cache` rebuilds the engine's cache state for each call from the reference's globals, which do not hold the stat index's file path |
| `tests/test_swift_cross_file_calls.py` | unchanged | `check_extract` |  |
| `tests/test_swift_import_resolution.py` | unchanged | `check_extract` |  |
| `tests/test_ts_abstract_methods.py` | unchanged | `check_extract` |  |
| `tests/test_ts_decorators.py` | unchanged | `check_extract` |  |
| `tests/test_ts_generators.py` | unchanged | `check_extract` |  |
| `tests/test_ts_import_require.py` | unchanged | `check_extract` |  |
| `tests/test_ts_import_type_arguments.py` | unchanged | `check_extract` |  |
| `tests/test_ts_inheritance.py` | unchanged | `check_extract` |  |
| `tests/test_ts_namespace.py` | unchanged | `check_extract` |  |
| `tests/test_ts_parse_warning.py` | unchanged | `check_extract` |  |
| `tests/test_typescript_enum_members.py` | unchanged | `check_extract` |  |
| `tests/test_zero_node_no_cache.py` | unchanged | `check_extract` | `test_zero_node_result_not_cached_then_self_heals`: forces a zero-node result by patching the reference's private `_safe_extract_with_xaml_root`; the engine has no Python seam to patch, so its extraction is unchanged |
