---
name: graphicle
description: Use Graphicle's MCP tools for questions about how this repository works, for changes that span several files, and to recall or record the project's decisions. Graphicle answers with path:line citations from the code graph, whole-repository packs and reviewed project memory.
metadata:
  display-name: Graphicle
---

# Graphicle

Graphicle indexes this repository into a code graph and keeps the project's
reviewed memory. Its context already arrived at the start of the session (a
map of the repository and the reviewed memories) and after code questions
(cited excerpts). Use its tools when that is not enough.

## Which tool

- **A question about how something works, across files:** call `pack` with
  the question and a budget (default 32000 tokens, at most 200000). It
  returns the most relevant code with line numbers, callers and callees,
  tests and docs, and a manifest of what it left out. Answer from it and
  cite `path:line`.
- **Where a symbol is defined, who calls it, what it calls:** `query_graph`
  with the name, then `get_node` and `get_neighbors`. `shortest_path`
  connects two symbols.
- **A past decision, a reason, a convention, or anything said in an earlier
  session:** `recall_project`, then `read_project_source` to quote the source
  it cites. `project_memory` lists the reviewed memories.
- **An exact string you already know:** search the files as usual; Graphicle
  is not needed.

## Recording a decision

When the person makes a decision worth keeping (a design choice, a
convention, a reason not to do something), call `propose_memory` with a
short title, the decision in a sentence or two, and the files or turns it
rests on. A proposal is a draft: a person approves it in Graphicle before
any session sees it. The next session starts with the approved memories.

## Citations

Cite every claim from Graphicle as `path:line` (code) or by the memory or
source id it gave. If a tool says the folder is not a Graphicle project,
tell the person to run `graphicle setup` there.
