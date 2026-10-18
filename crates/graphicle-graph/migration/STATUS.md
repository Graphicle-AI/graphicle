# Rust port of Graphify

Pinned source: Graphify 0.9.91, `10130e79ed9caf8f60d998073e097531eaa1a395`.

**88/95 application files currently verified** (78,578/86,217 source lines). Qualification is incomplete.

The inventory accounts for all **1068 archive files**. Copied documentation, parser fixtures and licenses retain their formats; they are not counted as converted application code.

| Archive category | Files |
| --- | ---: |
| application_python | 95 |
| build_or_ci_configuration | 14 |
| development_tool_python | 5 |
| documentation_or_prompt_asset | 350 |
| license_notice | 3 |
| other_asset_or_configuration | 19 |
| reference_fixture | 157 |
| upstream_test_python | 425 |

The 425 original Python test files are retained as behavioral references and are not part of the Rust rewrite, as requested on 23 September 2026. During qualification the comparison checks run the upstream test files that their receipts name against the native APIs, and compare with the pinned reference with the reviewed upstream fixes applied. Not every upstream test file is run: the repository's `docs/evidence/upstream-tests-not-run.md` lists those that are not, with the reason for each. 5 developer tools and 14 build/CI files remain separately accounted for.

The Rust library is linked into the memory core through `graph.*` dispatch. The native executable provides the CLI, MCP server, extraction/update/watch workflows, hooks and installer. Optional build features and external integrations are described in [README](../README.md). Python is not a runtime fallback.

Each current receipt binds the module, complete Rust source set, immutable binary and comparison tools. This report checks those hashes again. [Reviewed upstream fixes](upstream-review/REVIEW.md), [native hardening](HARDENING.md), and profile-driven optimization and measurements are separately documented. Behavior comparisons do not establish model-quality superiority.

| Application source | Lines | Status | Native files | Evidence |
| --- | ---: | --- | --- | --- |
| `graphify/__init__.py` | 30 | Verified | [src/lib.rs](../src/lib.rs), [src/model.rs](../src/model.rs), [src/error.rs](../src/error.rs), [tests/public_api.rs](../tests/public_api.rs) | receipt |
| `graphify/__main__.py` | 1,091 | Pending | — | — |
| `graphify/_minhash.py` | 107 | Verified | [src/minhash.rs](../src/minhash.rs) | receipt |
| `graphify/affected.py` | 393 | Verified | [src/affected.rs](../src/affected.rs), [src/graph.rs](../src/graph.rs), [src/session.rs](../src/session.rs), [src/graph_node_link.rs](../src/graph_node_link.rs) | receipt, receipt |
| `graphify/analyze.py` | 834 | Verified | [src/analyze.rs](../src/analyze.rs), [src/analyze_data.rs](../src/analyze_data.rs), [src/graph_algorithms.rs](../src/graph_algorithms.rs), [src/python_compat.rs](../src/python_compat.rs) | receipt |
| `graphify/benchmark.py` | 121 | Verified | [src/benchmark.rs](../src/benchmark.rs), [src/query_terms.rs](../src/query_terms.rs), [src/assets/query/stopwords.json](../src/assets/query/stopwords.json), [src/assets/query/jieba-py-0.46.12.hmm](../src/assets/query/jieba-py-0.46.12.hmm) | receipt |
| `graphify/build.py` | 2,806 | Verified | [src/build.rs](../src/build.rs), [src/build_normalize.rs](../src/build_normalize.rs), [src/build_merge.rs](../src/build_merge.rs), [src/build_api.rs](../src/build_api.rs), [src/build_global.rs](../src/build_global.rs), [src/build_labels.rs](../src/build_labels.rs), [src/hyperedge_members.rs](../src/hyperedge_members.rs) | receipt |
| `graphify/cache.py` | 1,939 | Verified | [src/cache.rs](../src/cache.rs), [src/cache_paths.rs](../src/cache_paths.rs), [src/cache_api.rs](../src/cache_api.rs) | receipt |
| `graphify/callflow_html.py` | 2,051 | Verified | [src/callflow_data.rs](../src/callflow_data.rs), [src/callflow_html.rs](../src/callflow_html.rs), [src/assets/callflow/templates.json](../src/assets/callflow/templates.json) | receipt |
| `graphify/cargo_introspect.py` | 166 | Verified | [src/cargo_introspect.rs](../src/cargo_introspect.rs) | receipt |
| `graphify/cli.py` | 5,364 | Verified | [src/cli.rs](../src/cli.rs), [src/cli_cluster.rs](../src/cli_cluster.rs), [src/cli_commands.rs](../src/cli_commands.rs), [src/cli_export.rs](../src/cli_export.rs), [src/cli_extract.rs](../src/cli_extract.rs), [src/cli_guard.rs](../src/cli_guard.rs), [src/cli_merge.rs](../src/cli_merge.rs), [src/cli_state.rs](../src/cli_state.rs), [src/assets/cli/constants.json](../src/assets/cli/constants.json), [src/assets/cli/help.txt](../src/assets/cli/help.txt) | receipt |
| `graphify/cluster.py` | 507 | Verified | [src/cluster.rs](../src/cluster.rs) | receipt |
| `graphify/cross_repo_calls.py` | 273 | Verified | [src/cross_repo_calls.rs](../src/cross_repo_calls.rs) | receipt |
| `graphify/cross_repo_types.py` | 75 | Verified | [src/cross_repo_types.rs](../src/cross_repo_types.rs) | receipt |
| `graphify/csharp_dispatch.py` | 39 | Verified | [src/csharp_dispatch.rs](../src/csharp_dispatch.rs) | receipt |
| `graphify/dedup.py` | 1,649 | Verified | [src/dedup.rs](../src/dedup.rs), [src/dedup_strings.rs](../src/dedup_strings.rs), [src/dedup_api.rs](../src/dedup_api.rs) | receipt |
| `graphify/detect.py` | 2,981 | Verified | [src/detect.rs](../src/detect.rs), [src/detect_types.rs](../src/detect_types.rs), [src/detect_ignore.rs](../src/detect_ignore.rs), [src/detect_documents.rs](../src/detect_documents.rs), [src/detect_manifest.rs](../src/detect_manifest.rs), [src/assets/detect/constants.json](../src/assets/detect/constants.json) | receipt |
| `graphify/diagnostics.py` | 422 | Verified | [src/diagnostics.rs](../src/diagnostics.rs), [src/diagnostics_report.rs](../src/diagnostics_report.rs), [build.rs](../build.rs) | receipt |
| `graphify/export.py` | 1,432 | Pending | — | — |
| `graphify/exporters/__init__.py` | 1 | Verified | [src/exporters/mod.rs](../src/exporters/mod.rs) | receipt |
| `graphify/exporters/base.py` | 14 | Verified | [src/exporters/base.rs](../src/exporters/base.rs) | receipt |
| `graphify/exporters/graphdb.py` | 210 | Pending | — | — |
| `graphify/exporters/html.py` | 781 | Verified | [src/exporters/html.rs](../src/exporters/html.rs), [src/exporters/common.rs](../src/exporters/common.rs), [src/assets/export_html/styles.html](../src/assets/export_html/styles.html), [src/assets/export_html/script.html.template](../src/assets/export_html/script.html.template), [src/assets/export_html/hyperedges.html.template](../src/assets/export_html/hyperedges.html.template), [src/assets/export_html/document.html.template](../src/assets/export_html/document.html.template) | receipt |
| `graphify/extract.py` | 10,974 | Verified | [src/extract.rs](../src/extract.rs), [src/extract_languages.rs](../src/extract_languages.rs), [src/extract_members.rs](../src/extract_members.rs), [src/extract_merge.rs](../src/extract_merge.rs), [src/extract_normalize.rs](../src/extract_normalize.rs), [src/extract_projects.rs](../src/extract_projects.rs), [src/extract_python.rs](../src/extract_python.rs), [src/assets/extract/constants.json](../src/assets/extract/constants.json), [src/cache.rs](../src/cache.rs), [src/cache_paths.rs](../src/cache_paths.rs), [src/extractors/resolution.rs](../src/extractors/resolution.rs), [src/extractors/parse_cache.rs](../src/extractors/parse_cache.rs) | receipt, receipt |
| `graphify/extractors/__init__.py` | 76 | Verified | [src/extractors/registry.rs](../src/extractors/registry.rs), [src/extractors/mod.rs](../src/extractors/mod.rs) | receipt |
| `graphify/extractors/apex.py` | 215 | Verified | [src/extractors/apex.rs](../src/extractors/apex.rs) | receipt |
| `graphify/extractors/base.py` | 153 | Verified | [src/extractors/base.rs](../src/extractors/base.rs), [src/extractors/builtins_data.rs](../src/extractors/builtins_data.rs), [src/path_syntax.rs](../src/path_syntax.rs) | receipt |
| `graphify/extractors/bash.py` | 627 | Verified | [src/extractors/bash.rs](../src/extractors/bash.rs) | receipt |
| `graphify/extractors/blade.py` | 67 | Verified | [src/extractors/blade.rs](../src/extractors/blade.rs), [src/regex_compat.rs](../src/regex_compat.rs) | receipt |
| `graphify/extractors/cobol.py` | 297 | Verified | [src/extractors/cobol.rs](../src/extractors/cobol.rs) | receipt |
| `graphify/extractors/commonlisp.py` | 540 | Verified | [src/extractors/commonlisp.rs](../src/extractors/commonlisp.rs) | receipt |
| `graphify/extractors/csharp.py` | 567 | Verified | [src/extractors/csharp.rs](../src/extractors/csharp.rs), [src/html_compat.rs](../src/html_compat.rs), [src/html_entities.json](../src/html_entities.json) | receipt |
| `graphify/extractors/dart.py` | 589 | Verified | [src/extractors/dart.rs](../src/extractors/dart.rs) | receipt |
| `graphify/extractors/dm.py` | 507 | Verified | [src/extractors/dm.rs](../src/extractors/dm.rs) | receipt |
| `graphify/extractors/elixir.py` | 464 | Verified | [src/extractors/elixir.rs](../src/extractors/elixir.rs) | receipt |
| `graphify/extractors/engine.py` | 10,054 | Verified | [src/extractors/engine.rs](../src/extractors/engine.rs), [src/extractors/engine_calls.rs](../src/extractors/engine_calls.rs), [src/extractors/engine_config.rs](../src/extractors/engine_config.rs), [src/extractors/engine_declarations.rs](../src/extractors/engine_declarations.rs), [src/extractors/engine_declarator.rs](../src/extractors/engine_declarator.rs), [src/extractors/engine_extra.rs](../src/extractors/engine_extra.rs), [src/extractors/engine_parser.rs](../src/extractors/engine_parser.rs), [src/extractors/engine_ruby.rs](../src/extractors/engine_ruby.rs), [src/extractors/engine_scope.rs](../src/extractors/engine_scope.rs), [src/extractors/engine_semantics.rs](../src/extractors/engine_semantics.rs), [src/extractors/engine_types.rs](../src/extractors/engine_types.rs), [src/assets/engine/sets.json](../src/assets/engine/sets.json), [src/assets/engine/configs.json](../src/assets/engine/configs.json) | receipt |
| `graphify/extractors/erlang.py` | 353 | Verified | [src/extractors/erlang.rs](../src/extractors/erlang.rs), [vendor/tree-sitter-erlang/SOURCE.json](../vendor/tree-sitter-erlang/SOURCE.json) | receipt |
| `graphify/extractors/fortran.py` | 431 | Verified | [src/extractors/fortran.rs](../src/extractors/fortran.rs), [src/process.rs](../src/process.rs) | receipt |
| `graphify/extractors/go.py` | 754 | Verified | [src/extractors/go.rs](../src/extractors/go.rs) | receipt |
| `graphify/extractors/json_config.py` | 276 | Verified | [src/extractors/json_config.rs](../src/extractors/json_config.rs) | receipt |
| `graphify/extractors/julia.py` | 498 | Verified | [src/extractors/julia.rs](../src/extractors/julia.rs), [src/extractors/engine_semantics.rs](../src/extractors/engine_semantics.rs) | receipt |
| `graphify/extractors/markdown.py` | 582 | Verified | [src/extractors/markdown.rs](../src/extractors/markdown.rs), [src/extractors/markdown_data.rs](../src/extractors/markdown_data.rs), [src/extractors/yaml_frontmatter.rs](../src/extractors/yaml_frontmatter.rs) | receipt |
| `graphify/extractors/models.py` | 131 | Verified | [src/extractors/models.rs](../src/extractors/models.rs) | receipt |
| `graphify/extractors/objc.py` | 813 | Verified | [src/extractors/objc.rs](../src/extractors/objc.rs), [src/extractors/engine_declarator.rs](../src/extractors/engine_declarator.rs), [src/extractors/resolution_paths.rs](../src/extractors/resolution_paths.rs) | receipt |
| `graphify/extractors/ocaml.py` | 357 | Verified | [src/extractors/ocaml.rs](../src/extractors/ocaml.rs) | receipt |
| `graphify/extractors/pascal.py` | 720 | Verified | [src/extractors/pascal.rs](../src/extractors/pascal.rs), [src/extractors/pascal_paths.rs](../src/extractors/pascal_paths.rs) | receipt |
| `graphify/extractors/pascal_forms.py` | 196 | Verified | [src/extractors/pascal_forms.rs](../src/extractors/pascal_forms.rs), [src/io_compat.rs](../src/io_compat.rs) | receipt |
| `graphify/extractors/powershell.py` | 601 | Verified | [src/extractors/powershell.rs](../src/extractors/powershell.rs) | receipt |
| `graphify/extractors/r.py` | 482 | Verified | [src/extractors/r.rs](../src/extractors/r.rs), [vendor/tree-sitter-r/SOURCE.json](../vendor/tree-sitter-r/SOURCE.json) | receipt |
| `graphify/extractors/razor.py` | 196 | Verified | [src/extractors/razor.rs](../src/extractors/razor.rs) | receipt |
| `graphify/extractors/resolution.py` | 4,232 | Verified | [src/extractors/resolution.rs](../src/extractors/resolution.rs), [src/extractors/resolution_facts.rs](../src/extractors/resolution_facts.rs), [src/extractors/resolution_js.rs](../src/extractors/resolution_js.rs), [src/extractors/resolution_paths.rs](../src/extractors/resolution_paths.rs), [src/extractors/resolution_types.rs](../src/extractors/resolution_types.rs), [src/extractors/pascal_paths.rs](../src/extractors/pascal_paths.rs) | receipt |
| `graphify/extractors/robot.py` | 297 | Verified | [src/extractors/robot.rs](../src/extractors/robot.rs), [src/assets/robot/languages.json](../src/assets/robot/languages.json) | receipt |
| `graphify/extractors/rust.py` | 906 | Verified | [src/extractors/rust.rs](../src/extractors/rust.rs) | receipt |
| `graphify/extractors/sln.py` | 92 | Verified | [src/extractors/sln.rs](../src/extractors/sln.rs), [src/fsutil.rs](../src/fsutil.rs) | receipt |
| `graphify/extractors/solidity.py` | 484 | Verified | [src/extractors/solidity.rs](../src/extractors/solidity.rs), [vendor/tree-sitter-solidity/SOURCE.json](../vendor/tree-sitter-solidity/SOURCE.json) | receipt |
| `graphify/extractors/sql.py` | 785 | Verified | [src/extractors/sql.rs](../src/extractors/sql.rs), [vendor/tree-sitter-sql/src/lib.rs](../vendor/tree-sitter-sql/src/lib.rs), [vendor/tree-sitter-sql/build.rs](../vendor/tree-sitter-sql/build.rs), [vendor/tree-sitter-sql/grammar/parser.c](../vendor/tree-sitter-sql/grammar/parser.c), [vendor/tree-sitter-sql/grammar/scanner.c](../vendor/tree-sitter-sql/grammar/scanner.c), [vendor/tree-sitter-sql/grammar/tree_sitter/parser.h](../vendor/tree-sitter-sql/grammar/tree_sitter/parser.h), [vendor/tree-sitter-sql/SOURCE.json](../vendor/tree-sitter-sql/SOURCE.json) | receipt |
| `graphify/extractors/terraform.py` | 462 | Verified | [src/extractors/terraform.rs](../src/extractors/terraform.rs), [vendor/tree-sitter-hcl/src/lib.rs](../vendor/tree-sitter-hcl/src/lib.rs), [vendor/tree-sitter-hcl/build.rs](../vendor/tree-sitter-hcl/build.rs), [vendor/tree-sitter-hcl/grammar/parser.c](../vendor/tree-sitter-hcl/grammar/parser.c), [vendor/tree-sitter-hcl/grammar/scanner.c](../vendor/tree-sitter-hcl/grammar/scanner.c), [vendor/tree-sitter-hcl/SOURCE.json](../vendor/tree-sitter-hcl/SOURCE.json) | receipt |
| `graphify/extractors/vbnet.py` | 433 | Verified | [src/extractors/vbnet.rs](../src/extractors/vbnet.rs), [vendor/tree-sitter-vb-dotnet/SOURCE.json](../vendor/tree-sitter-vb-dotnet/SOURCE.json) | receipt |
| `graphify/extractors/verilog.py` | 364 | Verified | [src/extractors/verilog.rs](../src/extractors/verilog.rs) | receipt |
| `graphify/extractors/zig.py` | 245 | Verified | [src/extractors/zig.rs](../src/extractors/zig.rs) | receipt |
| `graphify/file_slice.py` | 249 | Verified | [src/file_slice.rs](../src/file_slice.rs) | receipt |
| `graphify/global_graph.py` | 201 | Verified | [src/global_graph.rs](../src/global_graph.rs), [src/build_global.rs](../src/build_global.rs), [src/graph.rs](../src/graph.rs) | receipt |
| `graphify/google_workspace.py` | 237 | Verified | [src/google_workspace.rs](../src/google_workspace.rs) | receipt |
| `graphify/hooks.py` | 1,137 | Verified | [src/hooks.rs](../src/hooks.rs), [src/hooks_runtime.rs](../src/hooks_runtime.rs) | receipt |
| `graphify/ids.py` | 93 | Verified | [src/ids.rs](../src/ids.rs), [src/unicode15.rs](../src/unicode15.rs) | receipt |
| `graphify/ingest.py` | 383 | Verified | [src/ingest.rs](../src/ingest.rs), [src/ingest_api.rs](../src/ingest_api.rs) | receipt |
| `graphify/install.py` | 2,779 | Verified | [src/install.rs](../src/install.rs), [src/install_fs.rs](../src/install_fs.rs), [src/install_platforms.rs](../src/install_platforms.rs), [src/assets/install/SKILL.md](../src/assets/install/SKILL.md), [src/assets/install/constants.json](../src/assets/install/constants.json), [src/assets/install/pi/index.ts](../src/assets/install/pi/index.ts), [src/assets/install/pi/package.json](../src/assets/install/pi/package.json), [src/assets/install/workflows.md](../src/assets/install/workflows.md) | receipt |
| `graphify/interface_dispatch.py` | 195 | Verified | [src/interface_dispatch.rs](../src/interface_dispatch.rs) | receipt |
| `graphify/llm.py` | 3,747 | Pending | — | — |
| `graphify/manifest.py` | 4 | Verified | [src/manifest.rs](../src/manifest.rs) | receipt |
| `graphify/manifest_ingest.py` | 337 | Verified | [src/manifest_ingest.rs](../src/manifest_ingest.rs) | receipt |
| `graphify/markdown_resolution.py` | 231 | Verified | [src/markdown_resolution.rs](../src/markdown_resolution.rs) | receipt |
| `graphify/mcp_ingest.py` | 386 | Verified | [src/mcp_ingest.rs](../src/mcp_ingest.rs) | receipt |
| `graphify/multigraph_compat.py` | 212 | Verified | [src/multigraph_compat.rs](../src/multigraph_compat.rs), [src/graph.rs](../src/graph.rs) | receipt |
| `graphify/pascal_resolution.py` | 129 | Verified | [src/pascal_resolution.rs](../src/pascal_resolution.rs) | receipt |
| `graphify/paths.py` | 593 | Verified | [src/paths.rs](../src/paths.rs), [src/python_json.rs](../src/python_json.rs), [src/security.rs](../src/security.rs) | receipt |
| `graphify/pg_introspect.py` | 165 | Pending | — | — |
| `graphify/prs.py` | 808 | Pending | — | — |
| `graphify/querylog.py` | 80 | Verified | [src/querylog.rs](../src/querylog.rs) | receipt |
| `graphify/rcfile.py` | 252 | Verified | [src/rcfile.rs](../src/rcfile.rs), [src/par.rs](../src/par.rs) | receipt |
| `graphify/reflect.py` | 882 | Verified | [src/reflect.rs](../src/reflect.rs), [src/python_datetime.rs](../src/python_datetime.rs) | receipt |
| `graphify/report.py` | 408 | Verified | [src/report.rs](../src/report.rs) | receipt |
| `graphify/resolver_registry.py` | 88 | Verified | [src/resolver_registry.rs](../src/resolver_registry.rs) | receipt |
| `graphify/ruby_resolution.py` | 524 | Verified | [src/ruby_resolution.rs](../src/ruby_resolution.rs), [src/build_labels.rs](../src/build_labels.rs) | receipt |
| `graphify/scip_ingest.py` | 363 | Verified | [src/scip_ingest.rs](../src/scip_ingest.rs) | receipt |
| `graphify/security.py` | 461 | Verified | [src/security.rs](../src/security.rs), [src/ip_policy_data.rs](../src/ip_policy_data.rs), [src/decimal_data.rs](../src/decimal_data.rs) | receipt |
| `graphify/semantic_cleanup.py` | 336 | Verified | [src/semantic_cleanup.rs](../src/semantic_cleanup.rs), [src/hyperedge_members.rs](../src/hyperedge_members.rs) | receipt |
| `graphify/serve.py` | 3,007 | Verified | [src/serve.rs](../src/serve.rs), [src/serve_api.rs](../src/serve_api.rs), [src/serve_find.rs](../src/serve_find.rs), [src/serve_http.rs](../src/serve_http.rs), [src/serve_live.rs](../src/serve_live.rs), [src/serve_prs.rs](../src/serve_prs.rs), [src/serve_search.rs](../src/serve_search.rs), [src/serve_socket.rs](../src/serve_socket.rs), [src/serve_text.rs](../src/serve_text.rs), [src/assets/serve/constants.json](../src/assets/serve/constants.json) | receipt |
| `graphify/swift_dispatch.py` | 46 | Verified | [src/swift_dispatch.rs](../src/swift_dispatch.rs) | receipt |
| `graphify/symbol_resolution.py` | 577 | Verified | [src/symbol_resolution.rs](../src/symbol_resolution.rs) | receipt |
| `graphify/transcribe.py` | 186 | Pending | — | — |
| `graphify/tree_html.py` | 604 | Verified | [src/tree_html.rs](../src/tree_html.rs), [src/assets/tree.html.template](../src/assets/tree.html.template) | receipt |
| `graphify/validate.py` | 95 | Verified | [src/validate.rs](../src/validate.rs), [src/python_compat.rs](../src/python_compat.rs), [src/python_repr_data.rs](../src/python_repr_data.rs) | receipt |
| `graphify/watch.py` | 2,861 | Verified | [src/watch.rs](../src/watch.rs), [src/watch_paths.rs](../src/watch_paths.rs), [src/watch_reconcile.rs](../src/watch_reconcile.rs) | receipt |
| `graphify/wiki.py` | 455 | Verified | [src/wiki.rs](../src/wiki.rs) | receipt |
