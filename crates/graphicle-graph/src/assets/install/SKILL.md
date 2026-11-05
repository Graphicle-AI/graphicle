---
name: graphicle
description: Build, update, query, and export a knowledge graph of source code, documents, and other local or ingested material with the graphicle-graph executable.
---

Use this skill when the user invokes `/graphicle`, asks for a knowledge graph, or wants architecture and dependency context from an existing graph.

When you are choosing a tool yourself (not when the user invoked `/graphicle`), a graph is worth building when you expect many questions against this corpus, the corpus is large or unfamiliar, or the question needs links text search cannot see (documents to code, communities, import cycles). If a graph already exists, query it. With no graph, one targeted lookup in an ordinary-sized codebase is faster with a plain text search; mention that a graph can be built. Code extraction is fast and local; semantic extraction runs the model over every document.

Run `graphicle-graph --version` to check the installation. All processing runs in the native executable. Use `graphicle-graph --help` for the command inventory, and read [the workflow reference](references/workflows.md) for advanced operations.

1. Choose the input directory from the user's request; default to `.`. Pass it to commands as one single-quoted argument and never splice it into other shell code, so a path containing shell metacharacters cannot execute. Inspect an existing `.graphicle/GRAPH_REPORT.md` or query its graph before a rebuild.
2. For source code, run `graphicle-graph extract <path> --code-only`. This uses local parsers and requires no API key. For semantic extraction of documents and code explanations, run `graphicle-graph extract <path>` with the configured LLM provider. Honor the user's cost, privacy, and network constraints before contacting a provider.
3. Review the extraction diagnostics, failed sources, and incomplete-build warnings. Preserve the existing graph when extraction is incomplete. Use `--force` only when deliberate deletions or replacement explain a reduction.
4. Use `graphicle-graph query "<question>"`, `path "<A>" "<B>"`, `explain "<concept>"`, or `affected "<symbol>"` to obtain scoped context. Cite source paths, line numbers, edge relations, and confidence. Treat INFERRED relationships as hypotheses to verify in source material.
5. After editing code, run `graphicle-graph update <path>` to refresh the graph. Explain important architecture findings, uncertain edges, and remaining extraction failures to the user.

Exports include HTML, call-flow HTML, SVG, GraphML, Obsidian, wiki, Neo4j, and FalkorDB. The server command provides the same graph context over MCP. Saved answers and reflections create an explicit, inspectable work-memory loop.

The project graph belongs to the user. Follow their explicit instructions about graph usage, output directories, version control, provider access, and installation scope.
