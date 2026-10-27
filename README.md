<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.svg">
    <img src="assets/logo-light.svg" alt="Graphicle" width="300">
  </picture>
</p>

<h3 align="center">Code graph and memory for your coding agents, in one Rust program.</h3>

<p align="center">
  <a href="LICENSE"><img alt="License: Apache-2.0" src="https://img.shields.io/badge/license-Apache--2.0-8B5CF6?style=flat-square&amp;labelColor=1E2235"></a>
  <a href="https://github.com/graphicle-ai/graphicle/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/graphicle-ai/graphicle?style=flat-square&amp;color=5B7CFF&amp;labelColor=1E2235"></a>
  <img alt="Platforms: macOS, Linux and Windows (preview)" src="https://img.shields.io/badge/platforms-macOS%20%7C%20Linux%20%7C%20Windows%20%28preview%29-0EA5E9?style=flat-square&amp;labelColor=1E2235">
  <a href="https://discord.com/invite/rWuU7cWU4E"><img alt="Chat on Discord" src="https://img.shields.io/badge/chat-Discord-5865F2?style=flat-square&amp;labelColor=1E2235&amp;logo=discord&amp;logoColor=white"></a>
</p>

<p align="center">
  <a href="https://www.producthunt.com/products/graphicle?embed=true&amp;utm_source=badge-featured&amp;utm_medium=badge&amp;utm_campaign=badge-graphicle">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://api.producthunt.com/widgets/embed-image/v1/featured.svg?post_id=1294027&amp;theme=dark">
      <img alt="Graphicle on Product Hunt" src="https://api.producthunt.com/widgets/embed-image/v1/featured.svg?post_id=1294027&amp;theme=light" width="250" height="54">
    </picture>
  </a>
</p>

<p align="center">
  Graphicle maps your repository into a code graph, remembers your project's docs, decisions and past agent sessions,
  and gives your coding agent cited excerpts from both: the exact files and lines. It runs on your machine: no API key needed, no account, <a href="#privacy-and-local-first">no telemetry</a>.<br>
  <sub>Works with Claude Code · Cursor · Codex · VS Code · Claude Desktop · any MCP client</sub>
</p>

<p align="center">
  <a href="#quick-start">Quick start</a> · <a href="#how-it-works">How it works</a> · <a href="#benchmarks">Benchmarks</a> · <a href="docs/quickstart.md">Docs</a> · <a href="docs/migrate-from-graphify.md">Coming from Graphify?</a> · <a href="https://github.com/graphicle-ai/graphicle/discussions">Discussions</a>
</p>

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/hero-setup-dark.png">
    <img src="assets/hero-setup-light.png" alt="A terminal in ~/code/shop, a small Python repository. graphicle setup --yes syncs its 4 files, builds the code graph (11 nodes, 15 edges) and prints a summary: 4 files and 6 symbols; Claude Code, Codex, Cursor, VS Code and Claude Desktop added, one MCP server each with the code graph, recall and memory, and Claude Code given five of its tools; session capture added for Claude Code and Codex; AGENTS.md added; post-commit, post-merge and post-checkout Git hooks added; files that change are synced before each answer; no answer model, so graphicle ask shows the evidence without a written answer. Real output of Graphicle 0.1.8, cut after the summary." width="860">
  </picture>
</p>

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/speed-kubernetes-dark.svg">
    <img src="assets/speed-kubernetes-light.svg" alt="Questions on kubernetes: Graphicle 0.1.8 against 0.1.7. Time per question, the median seconds of a graphicle context call: 22 questions on 16 cores, 0.1.7 2.99 s, 0.1.8 1.34 s; 22 questions on 4 cores, 3.89 s and 1.38 s; 22 questions on 4 cores with the file cache dropped before each call, 4.55 s and 1.90 s; 8 lookups on 16 cores, 2.52 s and 0.96 s. Peak memory of a call, the median over the 22 questions on 16 cores: 3.5 GB and 0.6 GB. On the same store, both releases gave the same packet for every question, byte for byte. Our Linux server (Intel Xeon Platinum 8480C); kubernetes at ca8a2a9e4, a shallow clone of 30,315 files; the 22 questions of issue 8; each call a new process, as the prompt hook runs it; 25 October 2026." width="860">
  </picture>
  <br><sub>Graphicle 0.1.8 against 0.1.7 on kubernetes (a shallow clone at <code>ca8a2a9e4</code>), the 22 questions of <a href="https://github.com/graphicle-ai/graphicle/issues/8">issue #8</a>, our Linux server: a median of 1.34 s per <code>graphicle context</code> call on 16 cores, against 2.99 s, each call a new process. <a href="docs/benchmarks.md#our-own-run-on-kubernetes-017-and-018">Method and every row</a></sub>
</p>

## Quick start

```sh
brew install graphicle-ai/tap/graphicle
cd your-repo
graphicle setup
```

<sub>No Homebrew? <code>curl -fsSL https://github.com/graphicle-ai/graphicle/releases/latest/download/install.sh | sh</code> · [Release archives](https://github.com/graphicle-ai/graphicle/releases) · [Other ways to install](docs/install.md)<br>
Windows: a preview build for x86-64, not yet code-signed ([Windows (preview)](docs/install.md#windows-preview)).</sub>

`graphicle setup` indexes the repository, builds its code graph and connects the coding agents it finds. It also turns on session
capture, adds a two-line section to `AGENTS.md` (unless Claude Code is the only agent) and installs Git hooks that keep the project current. It lists every file it changes and
backs each one up first; `graphicle disconnect` undoes it all.

Then ask your agent to *"use graphicle to find where failed requests are retried"*, or ask from the terminal:

```console
$ graphicle ask "where is tax added to prices?"
Evidence for "where is tax added to prices?"
4 excerpts · 606 of 3,000 tokens

[1] shop/pricing.py:1-7  price_with_tax()
    """Prices and tax."""
    TAX_RATE = 0.2


    def price_with_tax(price):
        """A price with the sales tax added."""
        return round(price * (1 + TAX_RATE), 2)
```

<sub>Real output, cut after the first excerpt. With no model configured, <code>ask</code> shows the evidence; <code>graphicle config set model anthropic</code> (or <code>openai</code>, <code>ollama</code>) adds written answers.</sub>

<details>
<summary><b>Let your coding agent install it</b></summary>

Paste this into Claude Code, Codex or Cursor:

```text
Install Graphicle with `brew install graphicle-ai/tap/graphicle` (or, without Homebrew,
`curl -fsSL https://github.com/graphicle-ai/graphicle/releases/latest/download/install.sh | sh`),
then run `graphicle setup --yes` in this repository and list every file it changed.
```
</details>

## Start here

| You want to | Run |
|---|---|
| Give your coding agent the code graph and your project's memory | `graphicle setup` in the repository |
| Ask a question from the terminal | `graphicle ask "how are failed requests retried?"` |
| Browse the graph, the sources and the memory | `graphicle open` |
| See what a change to a symbol touches | `graphicle graph extract . --code-only`, then `graphicle graph affected "retry()"` |
| Check that everything works | `graphicle doctor` |
| Undo everything setup did | `graphicle disconnect` |
| Move from Graphify, or run a server for a team | [Moving from Graphify](docs/migrate-from-graphify.md) · [Team server](docs/team.md) |

## Why Graphicle

- **Native and fast.** One Rust program. On 13 codebases, Graphicle's 0.1.1 memory engine (a pre-release build) took 3.0 to 22.7 times less time than Graphify 0.9.91 in all 39 measurements (code-only extract, priming update and update with no change, on the same machine with 3 CPUs and 8 workers each). The graphs differ slightly: node counts within 4.7%, edge counts within 2.0% ([speed table](docs/benchmarks.md#speed-on-13-codebases-against-graphify-0991)). A later pre-release build with 0.1.1's memory changes, whose graphs also differ from Graphify's, used less peak memory than Graphify 0.9.91 in all 39 measurements, from 0.29 to 0.97 times Graphify's figure ([memory table](docs/benchmarks.md#peak-memory-against-graphify-0991)).
- **Cited context, within a budget you set.** Your agent asks Graphicle and gets the relevant code, docs and decisions as cited excerpts, within a token budget you set. In our tests it tied grep on finding code, and we do not claim it saves tokens ([what we measured](#benchmarks)).
- **Answers you can check.** Every excerpt names its file and lines. Memories an agent proposes wait for your approval.
- **One program, not a stack.** The code graph, the memory, one MCP server, the web app and the command line come in one install, with one SQLite file. No Python, Docker or database server.
- **Local and private.** No account and no telemetry. Your code and data leave your machine only when you ask. The only automatic request is a daily check for a new release, which `GRAPHICLE_NO_UPDATE_NOTIFIER=1` turns off.

## What it does

- **Code graph.** Files, functions, classes, calls and imports, parsed with tree-sitter and no model. `graphicle graph` searches it, explains a symbol,
  traces a path and exports an interactive HTML graph. After `graphicle graph extract . --code-only`, `graphicle graph affected "retry()"` lists what depends on a symbol.
- **Project memory.** Docs (Markdown, PDF, Office, HTML), reviewed decisions and procedures, and past Claude Code and Codex sessions, searchable alongside the code.
- **Cited context on a budget.** `graphicle context` and the MCP tools return excerpts with their sources, within a token budget, without calling a model.
- **One MCP server.** `graphicle mcp` serves the code-graph tools, under Graphify's tool names such as `query_graph`, `get_neighbors` and `shortest_path`, next to the memory tools. With `--pull-requests` it also serves the pull-request tools `list_prs`, `get_pr_impact` and `triage_prs`, which use the GitHub CLI ([MCP](docs/mcp.md)).
- **Memory tied to your code.** Reviewed memories are tied to the functions they are about: a memory goes stale when that code changes, and edits elsewhere in the file usually leave it current ([the exact rule](crates/graphicle/README.md#memory-anchored-to-code-symbols)). Mentions of code in your docs are linked to the definitions they name (`graphicle call graph.doc_links` lists them).
- **Across repositories.** Recall can span the repositories of a workspace: `graphicle context --workspace`, or [the workspace MCP endpoint](docs/mcp.md#several-projects-in-one-connection).
- **Always current.** Before each answer Graphicle syncs the few files you changed; Git hooks sync after each commit, merge and checkout.
- **Sessions carry over.** Session capture keeps each finished turn, and once you have reviewed memories, each new session starts with them and the latest turns.
- **An app.** `graphicle open` shows the graph, the sources and the memory in your browser. It is where you approve memories.

## Works with your agents

`graphicle setup` connects every client it finds over MCP. Each file is backed up once as `<file>.graphicle-bak`, and `graphicle disconnect` removes what it added.

| Client | Where setup connects it | Session capture |
|---|---|---|
| Claude Code | `.mcp.json` in the repository | hooks in `.claude/settings.local.json` |
| Codex | `~/.codex/config.toml` | hooks in `~/.codex/hooks.json` (Codex asks you to trust them) |
| Cursor | `~/.cursor/mcp.json` | |
| VS Code | `.vscode/mcp.json` in the repository | |
| Claude Desktop | `claude_desktop_config.json` in Claude's settings folder | |
| Any other MCP client | a short JSON entry ([MCP](docs/mcp.md)) | |

## How it works

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/how-it-works-dark.svg">
    <img src="assets/how-it-works-light.svg" alt="How Graphicle works: your code, documents and agent sessions sync into one program that keeps a code graph and a memory. Your coding agents use it over MCP; you use it with graphicle ask and graphicle open. Changed files sync before each answer. No account, no telemetry; your code and data leave your machine only when you ask, and the only automatic request is a daily check for a new release." width="820">
  </picture>
</p>

1. **Sync.** Graphicle reads the folder (it respects `.gitignore` and skips secrets), keeps a versioned copy of each source and builds the code graph.
2. **Recall.** When your agent calls a tool, Graphicle syncs what changed, ranks excerpts from code, documents, sessions and reviewed memory, and returns them with citations. The tools call no model; your agent's own model reads the excerpts.
3. **Remember.** Captured sessions become sources. Memories that agents propose wait for your approval in the app.

## Graphicle, or a code-graph tool plus a memory server

The left column is a code-graph tool, such as Graphify's open-source engine, plus a separate memory server. It is not a comparison with Graphify Cloud, a hosted service that includes memory.

| | A code-graph tool + a memory server | Graphicle |
|---|---|---|
| Install | Two tools, often on different runtimes, and often a database server | One install |
| Connect your agents | Configure each tool in each agent | `graphicle setup` connects the agents it finds; `graphicle disconnect` undoes it |
| MCP servers in your agent | Two, configured separately | One, with the graph and the memory tools |
| Storage | Graph files plus the memory server's database | One SQLite file in one data folder |
| Code structure: calls, imports, impact | The graph tool | `graphicle graph` |
| Docs, decisions, past sessions | The memory server | Built in |
| Citations and review | Varies | A file and its lines on every excerpt; agent memories wait for approval |

## Benchmarks

Every figure is labelled, and [docs/benchmarks.md](docs/benchmarks.md) has the full tables, methods and limits. In the agent tests, a difference counts only when its 95% interval excludes 0; otherwise it is a tie. Long sessions and memory built up across coding sessions are not tested yet.

- **Time per question** (0.1.8, the chart at the top). On kubernetes at `ca8a2a9e4` (a shallow clone, 30,315 files), over the 22 questions of issue #8, the median `graphicle context` call took 1.34 s with 0.1.8 against 2.99 s with 0.1.7 on 16 cores of our Linux server, 1.38 s against 3.89 s on 4 cores, and 1.90 s against 4.55 s on 4 cores with the file cache dropped before each call; 8 lookups took 0.96 s against 2.52 s on 16 cores. Each call was a new process, as the prompt hook runs it. On the same store, both releases gave the same packet for every question, and the peak memory of a call fell from 3.5 GB to 0.6 GB ([method and rounds](docs/benchmarks.md#our-own-run-on-kubernetes-017-and-018)).

### Finding the files a fix changes

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/retrieval-dark.svg">
    <img src="docs/assets/retrieval-light.svg" alt="Finding the files a fix changes: 100 SWE-bench Verified issues, held out from tuning. Top 1: Graphicle 0.1.8 43, BM25 (SWE-bench style) 35, grep for the issue's names 20; Top 5: Graphicle 0.1.8 75, BM25 (SWE-bench style) 61, grep for the issue's names 54; Top 10: Graphicle 0.1.8 81, BM25 (SWE-bench style) 77, grep for the issue's names 71; MRR: Graphicle 0.1.8 0.561, BM25 (SWE-bench style) 0.478, grep for the issue's names 0.365.">
  </picture>
</p>

**Offline ranking, 100 SWE-bench Verified issues never used for tuning.** Graphicle 0.1.8 put a file the fix changed in its first 5 files for 75 issues, against 61 for BM25 (SWE-bench style), a significant difference (McNemar exact p = 0.013); for the first file (43 vs 35) and the first 10 (81 vs 77) the differences are not significant. This measures ranking, not whether a coding agent fixes more issues; no agent test of this ranking has been run yet ([method, task lists and limits](docs/benchmarks/retrieval.md)).

### Speed, memory and agent tests of 0.1.0 and 0.1.1

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/graphicle-011-13-repos-dark.svg">
    <img src="assets/graphicle-011-13-repos-light.svg" alt="Two bar charts comparing Graphicle with Graphify 0.9.91 (Python) on 13 codebases. Left, speed: Graphify 0.9.91's time divided by Graphicle's for a code-only extract, highest first: kubernetes 22.7×, rails 7.0×, flask 6.9×, prometheus 6.6×, react 6.2×, scikit-learn 6.1×, pandas 5.8×, vscode 5.4×, ripgrep 5.3×, tokio 5.1×, redis 4.3×, next.js 4.0×, spring-boot 3.3×. A line on each bar spans its 3 operations (code-only extract, priming update and update with no change). Graphicle took less time in all 39 measurements and in every run, 3.0× to 22.7× less. Speed label: Graphicle's 0.1.1 memory engine (a pre-release build) against Graphify 0.9.91 on the same machine, 3 CPUs and 8 workers each, one session per program, 15 Oct 2026; median of 3 runs (1 for Kubernetes and VS Code). The graphs differ slightly: node counts within 4.7%, edge counts within 2.0%. Right, peak memory: Graphicle's peak divided by Graphify 0.9.91's peak for a code-only extract, lowest first: ripgrep 0.31×, flask 0.37×, pandas 0.38×, redis 0.39×, tokio 0.40×, scikit-learn 0.41×, prometheus 0.43×, react 0.50×, next.js 0.51×, vscode 0.52×, kubernetes 0.76×, rails 0.77×, spring-boot 0.94×. Lower in all 39 measurements, at 0.29× to 0.97× of Graphify's peak, on a pre-release memory build whose graphs differ from Graphify's; Graphicle's peak RSS or PSS, whichever is larger, against Graphify's peak PSS. A build of the merged 0.1.1 source stayed below in the 5 measurements rechecked. Speed and memory tables and method: docs/benchmarks.md." width="860">
  </picture>
</p>

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/graphicle-011-hero-dark.svg">
    <img src="assets/graphicle-011-hero-light.svg" alt="Four bar charts: Graphicle 0.1.1 against Graphify 0.9.91 (Python) and Claude Code alone. 1) Build the code graph, Kubernetes code-only extract, lower is better: Graphicle 0.1.1 (pre-release) 1 min 8 s, Graphify 0.9.91 25 min 47 s, 22.7× less time on this run. Graphicle's 0.1.1 memory engine, a pre-release build; one run each on the same machine, 3 CPUs and 8 workers each; the graphs differ slightly: node counts within 4.7%, edge counts within 2.0%. 2) Peak memory, Kubernetes code-only extract, lower is better: Graphicle 0.1.1 2,148 MiB (peak RSS, highest of 2 runs), Graphify 0.9.91 2,767 MiB (peak PSS). Lower in all 39 measurements (13 repositories × 3 operations, on a pre-release memory build; the graphs differ). 3) Facts found on Graphify's showcase corpus, 30 questions, higher is better: Claude Code alone 94.7%, Graphicle 0.1.1 99.2%, Graphify 0.9.91 87.8%. More than Graphify by 11.4 points, 95% CI [4.2, 19.7]; against Claude Code alone, a tie. 4) Tokens per answer in the same test, median, lower is better: Claude Code alone 60.8k, Graphicle 0.1.1 69.2k, Graphify 0.9.91 87.1k. Fewer than Graphify: mean difference −31.8k, 95% CI [−58.9k, −8.2k]; against Claude Code alone, no difference measured. Agent panels: Claude Code 2.1.287, Opus 5.5, effort medium; Graphicle 0.1.1 agent defaults (pre-release build); paired 95% CIs. Tests run 15 Oct 2026. Ties are shown as ties. Method and published results: docs/benchmarks.md." width="860">
  </picture>
</p>

- **Speed.** The 13-codebase chart and the first panel of the four-panel chart above: on 13 codebases, 3 operations each, Graphicle's 0.1.1 memory engine (a pre-release build) took 3.0 to 22.7 times less time than Graphify 0.9.91 in all 39 measurements, on the same machine with 3 CPUs and 8 workers each (one session per program; median of 3 runs, 1 for Kubernetes and VS Code). The graphs differ slightly: node counts within 4.7%, edge counts within 2.0% ([every measurement, with both graphs' sizes](docs/benchmarks.md#speed-on-13-codebases-against-graphify-0991)). [docs/benchmarks.md](docs/benchmarks.md) also has the earlier runs in which both programs built the same graph: the 0.1.0 engine against Graphify 0.9.88 on Kubernetes ([Kubernetes run](docs/benchmarks.md#speed-on-kubernetes)), and a pre-release build on Django, which ran a code-only extract in 7.5 s against 72.8 s for Graphify 0.9.77 (medians of five alternating runs, one shared Linux host, 8 workers; [the 8 October record](docs/benchmarks.md#speed-against-graphify-0977)).
- **Updates.** After each of 30 commits on prometheus, tokio and poetry, Graphicle 0.1.0's `graph update` took 6 to 8 times less time than Graphify 0.9.91's (one Linux node, 4 CPUs per run), and both matched a fresh build apart from three kinds of keying difference that both programs share. In a 30-note sample, the 15 notes whose function changed were flagged and no other; a wider exploratory check found some false flags ([details and limits](docs/benchmarks.md#updates-after-commits)).
- **Peak memory.** A pre-release build with 0.1.1's changes to how the engine holds its data, later than the one timed above, used less peak memory than Graphify 0.9.91 in all 39 measurements (13 repositories; extract and two kinds of update), for example 2,092 against 2,767 MiB on the Kubernetes extract and 1,426 against 2,800 MiB on the next.js extract. A build of the merged 0.1.1 source stayed below in the 5 measurements rechecked; the second panel of the four-panel chart shows its Kubernetes extract, 2,148 against 2,767 MiB (the higher peak of 2 runs). One Linux 8×H100 node, GPUs not used; 8 workers; Graphicle's peak RSS against Graphify's peak PSS ([every measurement](docs/benchmarks.md#peak-memory-against-graphify-0991)).
- **Finding code** (Claude Code 2.1.287 on a Linux node, Opus 5.5, effort medium). Graphicle ties grep. On 100 SWE-bench Verified tasks, plain Claude Code, Graphicle 0.1.0's default setup and Graphify 0.9.91 found every file the fix changed in 95, 95 and 94 of them; Graphicle used 16% and Graphify 29% more tokens than plain Claude Code, and the agent never called Graphicle's tools. On 93 tasks from our own code, which the model has not seen, all three tied again, and a prototype card from Graphicle 0.1.0's index (not shipped) used about 20% fewer tokens than Graphify.
- **Answering questions** (the same setup). On Graphify's showcase corpus (30 questions), the 0.1.1 agent defaults (a pre-release build) found 99.2% of the facts, against 94.7% for plain Claude Code (a tie) and 87.8% for Graphify 0.9.91: more facts than Graphify, with fewer tokens. On 60 questions about our own project all three tied, and Graphify used about 48k more tokens per question than Graphicle (pre-release 0.1.1 plus a prototype context hook) ([every agent test](docs/benchmarks.md#coding-agents-with-graphicle-graphify-or-neither)).
- **Agent memory.** In a comparison with seven memory systems, registered before any answer was scored and run with one open model as every system's reader and judge, a pre-release build of Graphicle trails none on three development sets. [Every score, with its method and caveats](docs/benchmarks.md#agent-memory-comparison-with-seven-memory-systems).

## FAQ

### Privacy and local first

- **One data folder.** Your sources, memories and the code graph that setup builds live in one SQLite store: `~/Library/Application Support/Graphicle` on macOS, `~/.local/share/graphicle` on Linux, `%LOCALAPPDATA%\Graphicle` on Windows. `graphicle graph extract` writes its graph file to `.graphicle/` in the project.
- **Loopback by default.** The app and its HTTP and MCP endpoints listen on 127.0.0.1.
- **No telemetry, no account.** Graphicle connects to the network only in these cases: a model or a connector you configure, a full `graphicle graph extract`
  (it sends documents and images to the LLM whose API key it finds), the GitHub CLI for `graphicle graph add`, `clone` and the graph server's pull-request tools, `graphicle update` with a feed you name, and a daily check for a new release.
  That check is on by default: a command you run in a terminal asks GitHub for the latest release, with no code or data, to tell you about a new one (`GRAPHICLE_NO_UPDATE_NOTIFIER=1` turns it off).
  A build from source also downloads the parser sources once. [Details](docs/faq.md)
- **Careful sync.** `sync` respects `.gitignore` and skips `.env` files, keys and credentials. Session capture drops `<private>…</private>` spans and redacts credentials.
- **Forgetting with a receipt.** Deleting a memory or a captured session also removes what was derived from it, and you get a hash-chained deletion receipt.

<details>
<summary><b>Do I need an API key or a model?</b></summary>

No, for most of it. The code graph (with `--code-only`), sync, `context`, the MCP server, session capture and the app work without one.
A model writes the answers of `graphicle ask`, proposes memories from sources and reads documents and images in a full `graphicle graph extract`.
Local models through Ollama or any OpenAI-compatible server work ([configuration](docs/configuration.md#models)).
</details>

<details>
<summary><b>Is Graphicle a fork of Graphify?</b></summary>

The code-graph engine is a Rust port of Graphify, credited in its file headers and in [NOTICE](NOTICE). It follows Graphify 0.9.95, and later Graphify releases are ported in later versions; [the parity checks](docs/benchmarks.md#parity-with-graphify) compare it with that release.
The memory, the memory tools, the app and session capture are not part of Graphify. Coming from Graphify? `graphicle setup` offers to switch a repository,
backs up each file and never deletes `graphify-out/` ([Moving from Graphify](docs/migrate-from-graphify.md)).
</details>

<details>
<summary><b>Will it handle my large repository?</b></summary>

By default a project can hold up to 100,000 text files and 1 GiB of text; you can raise both (see [Limits in docs/configuration.md](docs/configuration.md#limits)).
A very large file is analysed in part or kept as searchable text instead of stopping a sync.
</details>

<details>
<summary><b>Is there a version for teams?</b></summary>

Graphicle is free and open source under Apache-2.0, for individuals and teams alike. A commercial edition for teams is in private preview.
To talk to us, start a discussion: https://github.com/graphicle-ai/graphicle/discussions, or chat with us on [Discord](https://discord.com/invite/rWuU7cWU4E).
</details>

<details>
<summary><b>What is the licence?</b></summary>

Graphicle is Apache-2.0: free to use, change and ship, commercially too. Third-party parts keep their own licences, listed in [NOTICE](NOTICE).
</details>

More answers: [FAQ](docs/faq.md) · [Troubleshooting](docs/troubleshooting.md) · or run `graphicle doctor`.

## Documentation

[Quick start](docs/quickstart.md) · [Agents](docs/agents.md) · [MCP](docs/mcp.md) · [Install](docs/install.md) · [Configuration](docs/configuration.md) ·
[Team server](docs/team.md) · [Moving from Graphify](docs/migrate-from-graphify.md) · [Benchmarks](docs/benchmarks.md) · [FAQ](docs/faq.md) · [Changelog](CHANGELOG.md)

The code: [`crates/graphicle/`](crates/graphicle/README.md) (the program: command line, app, HTTP and MCP server), [`crates/graphicle-graph/`](crates/graphicle-graph/README.md) (the code-graph engine),
[`crates/graphicle-memory/`](crates/graphicle-memory/README.md) (the memory library), SDKs for [Python](sdk/python/README.md) and [TypeScript](sdk/typescript/README.md),
and a team server recipe in [`deploy/`](deploy/README.md). Each crate builds on its own ([building from source](docs/install.md)).

## Contributing

Contributions are welcome under the Apache License 2.0, with a Developer Certificate of Origin sign-off (`git commit -s`) and no CLA.
See [CONTRIBUTING.md](CONTRIBUTING.md). To report a security problem, follow [SECURITY.md](SECURITY.md) rather than opening a public issue.

## License and credits

Graphicle is licensed under the [Apache License 2.0](LICENSE). Copyright 2026 KarLex AI, Inc.

[Code signing policy](docs/code-signing.md): release builds are not yet signed with a trusted certificate. The Windows programs are signed through SignPath with a test certificate, which Windows does not trust, or left unsigned when that signing fails; each release's notes say which. Signing with the SignPath Foundation's certificate is planned. The policy also lists every case in which Graphicle contacts the network.

The code-graph engine is a Rust port of [Graphify](https://github.com/Graphify-Labs/graphify) by Safi Shamsi and the Graphify
contributors, licensed under Apache-2.0, with portions that remain available under the MIT License. Thank you.
[NOTICE](NOTICE), [NOTICE.md](NOTICE.md) and the `licenses/` folders hold the attributions and the licences of every adapted project and third-party component.

<p align="center"><br><b>If Graphicle is useful to you, star the repository so other developers find it.</b><br>
  <sub><a href="#quick-start">Try it</a> · <a href="https://github.com/graphicle-ai/graphicle/issues">Report a bug</a> · <a href="CONTRIBUTING.md">Contribute</a> · <a href="https://discord.com/invite/rWuU7cWU4E">Discord</a></sub><br>
  <sub>Built by <a href="https://www.karlexai.com">KarLex AI</a></sub>
</p>
