# Graphicle graph workflows

Build and update:

```sh
graphicle-graph extract . --code-only
graphicle-graph extract . --backend openai --model MODEL
graphicle-graph extract . --out OUTPUT_ROOT --no-gitignore
graphicle-graph update .
graphicle-graph cluster-only . --no-label
graphicle-graph label . --missing-only
graphicle-graph watch .
```

Semantic extraction supports the native provider registry, custom endpoints, chunking, bounded concurrency, checkpoint recovery, and per-source cache invalidation. Configure API keys through the provider's environment variable. Do not put credentials in graph data, committed files, or progress output. `--token-budget`, `--max-concurrency`, and `--api-timeout` control dispatch. Use `--allow-partial` only when explicitly accepting an incomplete result; inspect its recorded failed sources.

Focused navigation:

```sh
graphicle-graph query "How is authentication enforced?" --budget 2000
graphicle-graph path "src/auth.rs::authenticate" "Session"
graphicle-graph explain "Session"
graphicle-graph affected "authenticate" --depth 3 --relation calls
graphicle-graph god-nodes --top 10
graphicle-graph diagnose multigraph --json
graphicle-graph benchmark
```

Use `--graph PATH` to target another graph. Preserve provenance and distinguish extracted relationships from inferred ones. A missing graph result does not prove the underlying relationship is absent.

Content and repositories:

```sh
graphicle-graph clone https://github.com/OWNER/REPO --branch BRANCH
graphicle-graph add https://example.org/article --dir raw --author "Author"
graphicle-graph extract . --google-workspace
graphicle-graph extract . --postgres "$DATABASE_URL" --cargo
graphicle-graph global add .graphicle/graph.json --as PROJECT
graphicle-graph global list
graphicle-graph merge-graphs FIRST.json SECOND.json --out merged.json
```

Exports and work memory:

```sh
graphicle-graph export html --graph .graphicle/graph.json
graphicle-graph export callflow-html --lang auto
graphicle-graph export obsidian --dir vault
graphicle-graph export wiki
graphicle-graph export svg
graphicle-graph export graphml
graphicle-graph export neo4j
graphicle-graph export falkordb
graphicle-graph tree
graphicle-graph save-result --question "Question" --answer "Answer" --nodes SYMBOL --outcome useful
graphicle-graph reflect
```

MCP and maintenance:

```sh
graphicle-graph serve .graphicle/graph.json
graphicle-graph serve .graphicle/graph.json --transport streamable-http --host 127.0.0.1 --port 8080
graphicle-graph hook install
graphicle-graph hook status
graphicle-graph check-update .
```

Point the MCP client at the installed native executable with arguments `serve` and the absolute graph path. For HTTP authentication use `GRAPHICLE_MCP_API_KEY`. Scope an assistant installation with `graphicle-graph install --platform PLATFORM --project` or install its skill in the user configuration directory without `--project`.

Native library functions are also available through JSON Lines, with one request and one `{ok,result}` or `{ok,error}` response per line:

```sh
printf '%s\n' '{"operation":"extract","paths":["."],"root":"."}' | graphicle-graph --jsonl
```

The original parser, query, export, cache, reflection, and graph operations are callable through this boundary. No interpreter is needed for CLI, JSON Lines, Git hooks, or MCP.
