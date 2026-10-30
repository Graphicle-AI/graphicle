# Changelog

Notable changes to Graphicle: the graph engine (`graphicle-graph`), the memory
library (`graphicle-memory`) and the `graphicle` program.

## 0.1.10 - 2026-10-30

Every crate is at 0.1.10, and `graphicle --version` prints
`graphicle 0.1.10 (core)`. The Python and TypeScript SDKs are at 0.1.10
too.

Everything new in 0.1.10 was written during Meta's Global AI Developer
Hackathon.
[HACKATHON.md](https://github.com/Graphicle-AI/graphicle/blob/v0.1.10/HACKATHON.md)
lists it with when it was written.

### After upgrading

- Run `graphicle sync` once in each project, as after 0.1.9. The saved
  graph and the query file are keyed to the program's build, so the first
  sync writes both again. Until then a question reads the code graph from
  the store, and on a project of 2,000 files or more the prompt hook gives
  only the files that define the code names a prompt gives.
- The first check of a folder that an earlier version synced finds its
  HTML files changed, though nothing on disk changed, so the next command
  that reads the project (such as `graphicle pack`, `ask`, `context` or
  `status`, or an MCP tool call) syncs them again as they are (see the
  first bullet of Fixes). A folder with more than 200 such files, or one
  whose sync is slow, gets a note to run `graphicle sync` instead; with
  `freshness` set to `off`, run `graphicle sync` yourself.
- A reviewed project memory that cites a folder's HTML file goes stale
  after that sync, because the page's stored text changes from its words
  to its markup. Review those memories in the app (`graphicle open`,
  under **Memory**).
- In the GitHub Action of `graphicle review`, set `graphicle-version` to
  0.1.10, and `graphicle-sha256` to the SHA-256 of 0.1.10's archive for
  the runner (in the release's `SHA256SUMS`): the escaping under Fixes
  comes from the program the Action installs.

### Whole-repository packs

- Packs in the standard layout hold their top-ranked files whole more
  often. In a pack of 32,768 tokens or less, the first ranked file goes in
  whole when its estimated size fits in half the budget; in a pack of
  131,072 tokens or more, the first two go in whole when each one's
  estimated size fits in a quarter of it. The estimate comes from a file's
  size in bytes, not from the tokenizer. This changes `graphicle pack`, the
  MCP server's `pack` tool (32,000 tokens by default), and the packs of
  `ask --pack`, `plan` and `fix`, which use the standard layout by default.
  The default budget of `graphicle pack`, 128,000 tokens, stays below the
  second tier, and so do the packs for a model with a 128K window, unless
  a calibration from earlier calls raises their budget past 131,071
  tokens. A pack with pinned passages (`fix --focus`, `review`) keeps the
  per-file caps for every file, so each pinned passage keeps its room.
  This rule leaves packs of 32,769 to 131,071 tokens, and the cache
  layout, as they were.
- Measured without a model, as the share of questions (tasks, on
  SWE-bench Pro) whose pack holds every line that the reference fix
  changes, with tokens counted by the Muse Spark Open tokenizer: 0.1.9
  against a candidate build of 0.1.10 with both changes (whole top files,
  and a folder's HTML files kept as they are, under Fixes), made before
  this release's last fixes, at budgets of 16K to 1M tokens (16,384 to
  1,015,808; the 128K budget is 131,072 tokens, in the second tier). A
  build with those fixes, run again on the development questions, matched
  the candidate except for one question at 256K, whose pack left out one
  file the fix changed.
  - On the 90 held-out questions of the whole-repository benchmark
    (pytorch, vscode and elasticsearch, never tuned on): 26.7% to 34.4% at
    32K, 41.1% to 48.9% at 128K, 47.8% to 52.2% at 256K, 53.3% to 58.9% at
    512K and 58.9% to 64.4% at 1M; unchanged at 16K (21.1%) and 64K
    (33.3%). The mean share of the fix's files inside the pack (whole, or
    as excerpts or outlines) moved by +1.7 points at 16K, -0.3 at 128K and
    -1.1 at 512K, and not at the other budgets.
  - On 633 tasks of SWE-bench Pro's public set (at 1M, the 505 whose files
    hold more than 1M tokens): 10.3% to 15.0% at 16K, 16.7% to 20.4% at
    32K, 29.1% to 29.2% at 64K, 40.8% to 42.3% at 128K, 51.5% to 52.8% at
    256K and 66.5% to 68.3% at 1M, where file-level BM25 holds every
    changed line in 14.2%, 19.0%, 25.9%, 32.7%, 41.5% and 58.0% of tasks.
    The share of tasks with every file the fix changes inside the pack (any
    of its text) moved by -0.8 points at 16K and at 128K, -0.3 at 32K and
    -0.2 at 1M, and not at 64K and 256K. Counting only the files the pack
    holds as text, not as an outline, it fell by 2.1 points at 16K (95%
    interval -3.5 to -0.6) and by 0.3 points or less at the other budgets.
    A diagnosis of 0.1.9 on these tasks chose the directions of this
    change, though not its settings, so they are not an untouched test.
  - On the 200 development questions it was tuned on (kubernetes,
    home-assistant, angular, keycloak and godot): 32.0% to 36.0% at 16K,
    48.0% to 51.0% at 32K, 70.0% to 72.0% at 128K, 76.0% to 77.5% at 256K,
    79.5% to 81.0% at 512K and 81.0% to 82.5% at 1M; unchanged at 64K
    (57.5%). The mean share of the fix's files inside moved by -1.0 point
    at 32K and -0.5 at 1M, and by 0.2 points or less at the other budgets.

  The tables are in
  [bench/whole-repo/results-lines.md](https://github.com/Graphicle-AI/graphicle/blob/v0.1.10/bench/whole-repo/results-lines.md)
  and
  [bench/swe-bench-pro/results-e1l.md](https://github.com/Graphicle-AI/graphicle/blob/v0.1.10/bench/swe-bench-pro/results-e1l.md).

### Fixes

- A project folder's `.html` and `.htm` files are kept as they are on
  disk. Earlier versions stored the text that html2text extracted from
  them, so `graphicle pack` quoted that text under its own line numbers,
  not the file's, and a `graphicle fix` edit copied from that text could
  fail to match the file. Search and project memory read the same stored
  text, so they now see a page's markup as well as its words, and a hit or
  a quote points at the file's own line.
  - Office files, spreadsheets, PDF, EPUB and images are converted as
    before, and so is HTML that is uploaded or that the Confluence
    connector brings in. The GitHub and Google Drive connectors already
    kept `.html` files as they are.
  - A folder's HTML file that is not UTF-8, such as a Latin-1 or Shift-JIS
    page, or that is larger than `limits.file_bytes` (8 MiB by default), is
    still read as its extracted text, as before, up to
    `limits.document_bytes` (128 MiB by default).
  - An HTML file read as it is counts its whole markup, not just its words,
    toward `limits.project_bytes` (1 GiB by default), so a folder with many
    large pages comes closer to that limit; raise the limit if a sync is
    refused.
  - The notes-first ranking, which is off by default, no longer counts
    `.html` and `.htm` files as notes.
- `graphicle review --format github` and the GitHub Action escape `&`, `<`
  and `>` in the review's text outside code spans and code blocks, so
  markup in a model's comment (an `<img>` tag, or a `<!--` that would hide
  the rest of the review) shows as text in the posted review and in the job
  summary.
- `graphicle pack --format xml`: a file whose lines hold `</file` or
  `</section` (an HTML page's own `</section>`, for example) goes inside a
  CDATA section, so it no longer closes the pack's `<file>` or `<section>`
  element. Other files are unchanged, and the element names stay the same.

### Graph commands

- A `graphicle graph` command that reads the graph file, such as `query` or
  `affected`, now says how to build the file when it does not exist yet
  (reported by Thomas Thomsen,
  [#15](https://github.com/Graphicle-AI/graphicle/issues/15)). Before, it
  stopped with `graph file not found: <folder>/.graphicle/graph.json` and
  nothing more. Now, for the default graph file, the next line gives the
  command to run in that folder, `graphicle-graph extract . --code-only`
  (the same as `graphicle graph extract . --code-only`). The code graph
  that `graphicle setup` builds is in Graphicle's data folder, for the MCP
  server and `graphicle context`; `graphicle graph` reads its own file.

### Model profiles

- A model profile with an API key (`key_env`) can now use plain HTTP to a
  model server on your own network, such as a vLLM host without TLS, when
  you list that server's exact host in the new environment variable
  `GRAPHICLE_PROVIDER_HTTP_KEY_HOSTS` (the same format as
  `GRAPHICLE_PROVIDER_HTTP_HOSTS`; a host listed there also counts as
  listed in the older variable). The key then crosses the network in clear
  text. The variable is unset by default, so such a profile is still
  refused until you list its host; Graphicle prints a one-line warning
  that names such a host when it accepts one, and connectors and sign-in
  still need HTTPS or loopback (asked in discussion #16).

### Benchmark kits

- `bench/swe-bench-pro/` measures, without a model, how often a context
  holds every file and every line that a fix changes, on 633 tasks of
  SWE-bench Pro's public set: the pack, file-level BM25 and the repository
  cut to the same budget (`e1.py`, `e1l.py`, their reports and
  comparisons). It also holds the scripts of the model runs (`e2.py`,
  answers on a sample of the tasks; `c4c.py`, a billed prompt-cache meter;
  `live.sh`), whose results are not in this release. Results:
  [results-e1.md](https://github.com/Graphicle-AI/graphicle/blob/v0.1.10/bench/swe-bench-pro/results-e1.md)
  (files, 0.1.9) and
  [results-e1l.md](https://github.com/Graphicle-AI/graphicle/blob/v0.1.10/bench/swe-bench-pro/results-e1l.md)
  (lines, 0.1.9 and the 0.1.10 candidate build).
- `bench/whole-repo/lines.py` counts the changed lines that each context
  holds for the whole-repository questions, from each question's pull
  request diff placed on its snapshot, and compares two builds question by
  question:
  [results-lines.md](https://github.com/Graphicle-AI/graphicle/blob/v0.1.10/bench/whole-repo/results-lines.md).
- `bench/whole-repo/` gains two arms for its model runs (file-level BM25,
  and the agent with a pack in its first message) and two settings for
  the agent (a cap on the input tokens of all its turns, and Meta's
  hosted tool search), and `score.py` reports the cost per correct answer
  and McNemar's test on hit@1. The model runs have not been made yet.

### Packaging

- `server.json` at the repository's root is the MCP Registry entry of
  0.1.9, the latest listed release (it named 0.1.8). A release's entry is
  committed after its listing, as
  [packaging/mcpb/README.md](https://github.com/Graphicle-AI/graphicle/blob/v0.1.10/packaging/mcpb/README.md#steps-at-release-time)
  describes.

## 0.1.9 - 2026-10-28

Every crate is at 0.1.9, and `graphicle --version` prints
`graphicle 0.1.9 (core)`. The Python and TypeScript SDKs are at 0.1.9 too.

Most of what is new was written for Meta's Global AI Developer Hackathon.
[HACKATHON.md](https://github.com/Graphicle-AI/graphicle/blob/v0.1.9/HACKATHON.md)
says what was written before the event and what during it.

### How the new model features were tested

- The new commands that call a model (`ask --pack`, `plan`, `fix`,
  `review`, `pixel` and `see`) were tested only against stand-ins: local
  test servers that answer as the Meta Model API and OpenAI-compatible
  servers do, and `demo/mock_meta.py`. That covers the prompt cache
  requests, the exact counts of `POST /responses/input_tokens`, SAM 3.1 and
  computer use. None of them has called the live Meta Model API yet.
- The Muse Code plugin was tested with a stand-in for the `muse` program,
  not with Muse Code itself.
- The GitHub Action of `graphicle review` was tested with stand-ins for
  `curl`, `graphicle` and `gh`, not on GitHub's runners.
- The hosted demo in `demo/` was tested against `demo/mock_meta.py`. It is
  not deployed.

### After upgrading

- Run `graphicle sync` once in each project, as after 0.1.8. The saved
  graph and the query file are keyed to the program's build, so the first
  sync writes both again. Until then a question reads the code graph from
  the store, and on a project of 2,000 files or more the prompt hook gives
  only the files that define the code names a prompt gives.

### Whole-repository packs

- `graphicle pack [QUESTION]` gathers a project's code for one question into
  one block of text within a token budget (`--budget`, default 128,000, up
  to 2,000,000): a header, a manifest of what is in and the top files left
  out, an architecture summary, the ranked files (small ones whole, large
  ones as symbol excerpts with merged line ranges), callers and callees two
  steps along call edges, tests, docs and more of the repository. It reads
  `graphicle context`'s own ranking and the saved code graph. Formats:
  Markdown, XML, or JSON (`graphicle.pack.v1`, whose chunks' text joined in
  order is the pack). Without a question, an overview. See
  [docs/pack.md](https://github.com/Graphicle-AI/graphicle/blob/v0.1.9/docs/pack.md).
- A pack's ranking is `context`'s, read deeper (3,000 fused passages), with
  two additions: the files of a small folder the question names (by a word,
  a path or module segment, or words joined by `_`) right after the top
  files, and the files that define what the top files use (Go identifiers
  through their imports, Python modules of the same folder). Up to 256,000
  tokens, the ranks past 15 go in as compact excerpts (their passages and
  the first lines of the symbols around them) instead of more of the
  repository by centrality, and no file takes more than a tenth of the
  budget. The manifest lists up to 100 top-ranked files left out.
- A pack's ranking reads an issue without its environment sections (a
  heading below the title that labels the system information, tested
  versions, a `<name> version`, the environment or the operating system, or
  an issue form's question for them) and its `<name> version: <value>`
  lines; a title or a heading that merely names a version stays.
- The first pack after an upgrade writes the project's query file for the
  new build once, and says so; later packs read it.
- The budget is never exceeded: tokens are counted exactly with
  `--tokenizer tokenizer.json` (any Hugging Face tokenizer; the released
  program includes the pure-Rust `tokenizers` library for it), else
  estimated from bytes per token by language, calibrated on Muse Spark
  Open's counts (7% above them in 22 packs of 128,000 tokens), with a
  margin. The same inputs give the same bytes.
- `graphicle pack --layout cache` splits a pack for a provider's prompt
  cache. Its repository part comes first and is the same bytes for every
  question about a commit at the same budget and settings: a header without
  the question, the architecture summary, a map of the folders (files,
  tokens and most connected symbols of each), the files with the most
  connections in the code graph (whole, or the first line of each symbol) and
  the top documentation. The question part follows (a manifest, focus, call
  paths and tests) and never repeats lines the repository part holds: it
  lists the ranked files already there as `in the prefix: path lines a-b`,
  and gives full excerpts to one ranked file per 8,000 tokens of its room
  (each up to two fifths of the room divided among them, or 2,000 tokens
  when that is more) and compact excerpts to the ranks after them.
  `--prefix-share F` (default 0.9) is the repository part's share of the
  budget at most; 1.0 makes a stable pack, where only the question after it
  changes. See
  [docs/pack.md](https://github.com/Graphicle-AI/graphicle/blob/v0.1.9/docs/pack.md#layouts-and-the-prompt-cache).
- The pack JSON records the `layout`, `prefix_share` (cache layout only),
  `prefix_tokens`, `prefix_sha256` and a `part` (`prefix` or `suffix`) on
  every section; the prefix sections' text is the start of the pack's text.
- The MCP server publishes a `pack` tool (default 32,000 tokens, at most
  200,000) after the tools of 0.1.0's server. Claude Code's essential set
  stays five tools; `graphicle connect claude-code --tools all` adds `pack`.

### Ranking for TypeScript, Java and C++

- The pack's ranking was tuned on TypeScript, Java and C++ repositories
  (angular, keycloak and godot) as well as on Go and Python ones
  (kubernetes and home-assistant):
  - it reads an issue without its environment sections, as above, whose
    words (graphics drivers, display servers, package versions) pulled
    unrelated files up;
  - files the question names by a code name (`TextEdit` names
    `text_edit.cpp` and `text_edit.h`, `FormControl` names
    `form_control.ts`, `UserModel` names `UserModel.java`), or by two or
    three of its words in the languages that name files after their classes
    (Java, TypeScript and JavaScript, C and C++), come right after the top
    five files: at most 4 files per name, 4 names and 8 files;
  - TypeScript and JavaScript imports lead to the files that define what the
    top files use: relative paths (an ES module's `./x.js` is `x.ts`), index
    files and `tsconfig` path aliases.
- Measured without a model, as the mean share of the files a fix changed
  that are inside the pack, whole or as excerpts or outlines of their
  symbols (standard layout, tokens counted with the Muse Spark Open
  tokenizer), over the 90 questions of three repositories held
  out from tuning (pytorch, vscode and elasticsearch, 30 each): 58.1%,
  64.3%, 64.6%, 70.9% and 77.9% at budgets of 64K, 128K, 256K, 512K and 1M
  tokens, against 55.8%, 63.6%, 61.4%, 68.0% and 76.8% for the pack as it
  stood before the hackathon. pytorch gained 2.3 to 10.6 points; vscode
  lost 6.6 points at 64K, 0.9 at 256K and 3.3 at 512K. These numbers were
  measured before a later change to how a pack reads an issue's
  environment sections (the rule above). That change leaves the results of
  all 200 development questions unchanged; the held-out questions were not
  measured again. The method is that of
  [docs/benchmarks/whole-repo.md](https://github.com/Graphicle-AI/graphicle/blob/v0.1.9/docs/benchmarks/whole-repo.md#context-recall-without-a-model),
  whose table is of the earlier pack.

### Answers, plans and fixes with Meta's models

- `graphicle config set model meta` writes a Muse Spark profile for the
  Meta Model API (`https://api.meta.ai/v1`, its Responses API, key in
  `MODEL_API_KEY`, model `muse-spark-1.3`, `reasoning_effort` medium, sent
  as `reasoning.effort`) and makes it the answer model;
  `meta:muse-wonder-1.0` chooses Muse Wonder. It refuses the `-contributor`
  models, which let Meta train on the data, and a `reasoning_effort` of
  `none`, which the Meta Model API refuses. `graphicle config set model
  compatible:MODEL --url URL` does the same for any OpenAI-compatible
  server, such as Muse Spark Open on vLLM or SGLang. The docs give Meta's
  list prices for Muse Spark 1.3 and Muse Wonder (26 October 2026) as
  configuration examples; prices stay out of the program.
- Model profiles take input limits up to 1,048,576 tokens and output limits
  up to 131,072, and an optional `price` (US dollars per million input,
  output and cached input tokens). `ask --pack` and `plan` show the tokens
  in and out from the provider's usage, and a cost only from that price.
- `graphicle ask --pack` answers in one model call from a pack that fills
  the model's input limit, streams the answer from chat models, checks every
  cited `path:line` against the working tree and lists the ones that do not
  hold. After each call it keeps the provider's prompt tokens over its own
  estimate per model profile, and sizes the next pack to fill the input
  limit by the provider's count. The `basic` and `reflect` workflows are
  unchanged.
- `graphicle plan "change request"` asks for a plan as JSON (steps, files to
  edit with line ranges, new files, tests, risks), checks it against the
  working tree and prints it as Markdown or JSON. It changes no files.
- `graphicle ask --pack`, `graphicle plan` and `graphicle fix` use the
  standard layout by default; `--layout cache` and `--prefix-share` choose
  the cache layout, which pays off when you ask several questions about
  the same commit. A cache-layout request sends the same instructions
  first, then the pack, then the question. Without `--budget`, the
  repository part is sized from the model's input limit, not the
  calibrated budget, so it stays the same bytes from one question to the
  next; when the provider counts more than the estimate, it steps down to
  0.8, 0.64, ... of that size, the first whose share fits, and holds within
  each step. For the Meta Model API the request carries a
  `prompt_cache_key` built from the project, its commit and the pack's
  prefix hash (never from the question) and, on the Responses API, a
  24-hour `prompt_cache_retention`; other profiles opt in with
  `"prompt_cache": true`.
- Every request is fitted to the input limit: its estimate, times the
  profile's calibration, must fit, and a request near the limit is counted
  exactly with `POST /responses/input_tokens` for the Meta Model API; the
  pack is made again smaller while it would not fit. A profile's `tokenizer`
  (a `tokenizer.json`, such as Muse Spark Open's) makes these packs count
  exactly without the API.
- `graphicle fix "issue"` (or `--issue-file`) makes a pack and a plan. The
  pack is in the standard layout by default, since one run asks about one
  issue and a shared repository part would only take room from the files
  the issue needs; `--layout cache` or `--prefix-share` keeps the cache
  layout. It asks for the edits as search-and-replace blocks and new files,
  checks each round's edits as a whole (paths, search texts, and new files
  against each other and the folders on disk) and applies them all or none,
  putting back every file of the round if a write fails, on a new branch in
  a new git worktree from HEAD, made once the plan has come back. It runs
  `--test CMD` (or the configuration's `fix.test`) there with a timeout
  (`--timeout`, 1 to 86,400 seconds), and sends a failure's output and the
  diff back for at most `--rounds` rounds (default 3), each request fitted to
  the input limit. It reports the branch, worktree, files changed, each
  round's result, tokens and cost, and the cited lines; `--commit` commits
  on the new branch. `--focus PATH[:LINE|:START-END]` (up to 50) puts
  passages first in the pack's ranking, whatever the issue's words rank:
  PATH alone is the whole file, PATH:LINE the 40 lines on each side. Edits
  outside the repository (absolute paths, `..`, any spelling of `.git`,
  symlinks leading out or into `.git`) are refused; git runs with hooks off
  and without variables that point it elsewhere or add configuration (your
  configuration files, and so your commit identity, are kept); the test
  command never comes from the model and runs without the model key
  variables of every profile, those stored in the workspace too, whose
  values are hidden in its output; on Linux, Graphicle makes itself not
  dumpable before the test starts, so the test cannot read Graphicle's
  environment through `/proc`. The test still runs as you and can read what
  you can: use a container or virtual machine for repositories you do not
  trust. Nothing is pushed. See
  [docs/fix.md](https://github.com/Graphicle-AI/graphicle/blob/v0.1.9/docs/fix.md).
- The spend log: every model call of `ask --pack`, `plan`, `fix`, `review`,
  `pixel` and `see` adds one line to `usage.jsonl` in the data folder
  (tokens in, cached, out and reasoning, seconds, the cost at the profile's
  configured price, status; never the prompt or the answer). `graphicle
  usage` adds it up by day and model, with the cached share; `--since` and
  `--json`. After each answer, one line gives the tokens in (cached, with
  their share), out, and the cost.
- Cost caps in a profile, in US dollars at its configured price:
  `max_cost_per_call` refuses a call whose worst case (the whole input
  uncached, by the provider's count or the calibrated estimate plus 10%, and
  the output limit) costs more, and `max_cost_per_day` refuses a call whose
  worst case would take the day's cost of the profile past it. Calls not
  logged yet count at their worst case (held in `usage-pending.json`), so
  calls at the same time count against each other; calls without reported
  usage count at their worst case; a spend log that cannot be read refuses
  capped calls. Each refusal names the setting. A held worst case counts
  whatever its day (a call running past midnight UTC counts in the new
  day) and ends when its call is logged, its process ends, or after 40
  minutes. `graphicle usage` lists the calls not logged yet and the log
  lines it skipped; `--clear-pending` removes the held worst cases.

### Review a change: `graphicle review` and its GitHub Action

- `graphicle review` asks a long-context model (Muse Spark by default, with
  the `meta` profile) to review what a branch adds since its merge base with
  the base branch, as a pull request shows it. One model call gets a
  cache-layout pack whose ranking puts the changed hunks first, so it holds
  the changed code, the callers and callees of the changed symbols and
  their tests, then the architecture lines of the modules the change
  touches, the changed hunks and the diff. The model returns comments with
  a severity and `path:line` evidence. A comment is kept only on a changed
  line of the head with at least one citation that holds in the working
  tree; it is flagged when no citation holds, and dropped when it is on
  another file, outside the changed hunks, empty, a repeat or past
  `--max-comments` (20 by default). Formats: `markdown`, `json`, and
  `github`, the body of GitHub's create-a-review call, always a `COMMENT`
  that never approves or requests changes. It changes no files and posts
  nothing. See
  [docs/review.md](https://github.com/Graphicle-AI/graphicle/blob/v0.1.9/docs/review.md).
- A composite GitHub Action in `packaging/github-action/` runs `graphicle
  review --format github` on a pull request and posts one review. It
  installs a Graphicle release pinned by version and SHA-256, runs on the
  `pull_request` event only (it skips pull requests from forks, which get no
  secrets, and refuses `pull_request_target`), gives the model key only to
  the review step and the GitHub token only to the posting step, and refuses
  a call whose worst case costs more than `max-cost-usd` (0.50 US dollars by
  default, at Muse Spark 1.3's list price of 26 October 2026). 0.1.9 is the
  first release it can install. See
  [the GitHub Action](https://github.com/Graphicle-AI/graphicle/blob/v0.1.9/docs/review.md#the-github-action).

### From the screen to the code: `see`, `pixel` and the flow

- `graphicle pixel SCREENSHOT --at X,Y` finds the code that draws the
  element at a point of a screenshot. SAM 3.1 (`sam-3.1` on the Meta Model
  API) finds the element's box, a vision model (Muse Spark by default) reads
  its strings, and Graphicle searches the project for them as string
  literals (exact, then with mnemonics, ellipses and case set aside, then
  with placeholders), follows the keys of `package.nls.json` and of English
  JSON locale files to where they are used, adds the code graph's
  symbols named like the element, and ranks the candidates as `path:line`.
  `--text` gives the strings and makes no model call; `--explain` asks the
  model to rank the top candidates again from a small pack and explain
  them. SAM's calls are priced per image (`price.per_image`).
- `graphicle see` lets a model drive a web app in a browser you run, with
  computer use on the Meta Model API (Muse Wonder or Muse Spark 1.3), until
  the bug an issue describes shows (or, with `--look-for expected`, the
  expected behaviour). A driver command of yours runs the model's clicks and
  keys and returns screenshots; `demo/browser_driver.py` is one, headless
  Chromium that loads only the app's origin, blocks service workers and
  closes WebSockets to other origins. Each request replays the run so far,
  the model's encrypted reasoning included. It keeps every screenshot and a
  step log, and ends with a point on the last screenshot for `graphicle
  pixel`. A run takes at most 29 model calls (`--steps`, 25 by default). A
  pending safety check stops the run unless `--acknowledge-safety-checks`
  is given, and each acknowledgement is logged; the driver gets no model
  key; each call is checked against the profile's caps and the run's
  `--max-cost` (3 US dollars by default), and a profile without a price is
  refused. See
  [docs/flow.md](https://github.com/Graphicle-AI/graphicle/blob/v0.1.9/docs/flow.md).
- `demo/flow/flow.py` takes one bug from the screen to a tested fix as one
  command: `see` reproduces it, `pixel` finds the code, `fix --focus`
  patches it on a new branch and runs the app's tests, and `see --look-for
  expected` checks the patched app. It writes a receipt with the steps,
  citations, diff, the end of the test log, the screenshots before and
  after, and each call's tokens and cost. One budget holds for the whole
  run (`--max-run-cost`, 3 US dollars by default). It has run end to end
  only against `demo/mock_meta.py`, on a small app of ours and on
  excalidraw issue 12053, not yet against Meta's live API.
- `demo/` also holds a small web app for trying Graphicle with Meta's models
  on kubernetes, pytorch and vscode: a question answered from a cache-aware
  pack, and a click on a VS Code screenshot answered by `graphicle pixel`.
  It runs the `graphicle` program with fixed arguments and never exposes
  Graphicle's own local server. See
  [demo/README.md](https://github.com/Graphicle-AI/graphicle/blob/v0.1.9/demo/README.md).

### Muse Code

- `graphicle setup` connects Muse Code: it finds it by its settings folder,
  `~/.config/muse`, or the `muse` program, and adds Graphicle's MCP server
  to the `mcp_servers` of `~/.config/muse/settings.json`, with
  `"mode": "optional"`. `graphicle disconnect muse` removes it. See
  [docs/muse-code.md](https://github.com/Graphicle-AI/graphicle/blob/v0.1.9/docs/muse-code.md).
- A Muse Code plugin, as a preview: the MCP server, a skill and hooks in one
  package. With the `muse` program on your `PATH` (or in `~/.local/bin`),
  `graphicle connect muse --hooks` writes it into Graphicle's data folder,
  installs it with `muse plugins install --scope user` (or updates it) and
  removes the settings entry it replaces; approve it once with `muse
  plugins approve graphicle`. Setup does not install it: the plugin was
  tested against a stand-in `muse` program only, and stays opt-in until a
  check with Muse Code itself has passed. Run `graphicle disconnect muse`
  before you uninstall Graphicle or delete its data folder: it is not
  confirmed that Muse Code skips a plugin's server that fails to start.
- The plugin's hooks are built to add context without a tool call: at the
  start of a session, the project's map (the repository part of a
  cache-layout pack) and its reviewed memories; for a code question, the
  cited excerpts `graphicle context` finds (at most 8,400 characters);
  after a batch of tool calls that searched the files, one sentence on
  Graphicle's graph tools (at most five times a session). They keep the
  start of each request the same bytes through a session, so that the
  prompt cache can keep serving it; which hook events reach the model in
  Muse Code itself is not yet observed. The hooks never block Muse Code:
  each exits 0, and the session start waits at most 2.5 seconds for its
  context.
- Session memory with the plugin: the `Stop`, `PreCompact` and `SessionEnd`
  hooks hand Muse Code's session log to `graphicle sessions import`, so
  finished turns become sources of the project, and a decision one session
  records with `propose_memory` reaches the next session's start once a
  person approves it.
- `graphicle mcp --or-workspace`, the plugin's server, starts in any folder:
  it serves the project of that folder, else every project of the
  workspace.

### Fixes

- `graphicle setup` no longer stops when it cannot write a Git hook, as in
  Claude Code's sandbox, which does not let it write `.git/hooks`,
  `.mcp.json` or `.claude/settings.local.json` (reported by Thomas Thomsen,
  [#15](https://github.com/Graphicle-AI/graphicle/issues/15)). Before, it
  stopped with "Local I/O failed: Operation not permitted (os error 1)"
  without naming the file, and printed no summary, so the line to add to
  `.mcp.json` by hand was lost too. Now each hook it cannot write is listed
  with the reason, the summary gives the line to add to the hooks by hand,
  and setup goes on with its other steps. `AGENTS.md`, `CLAUDE.md` and the
  Graphify entries that setup removes are handled the same way, and an
  agent's settings file that cannot be backed up still gets the entry to
  paste into it. A hooks file that setup cannot write, Claude Code's
  `.claude/settings.local.json` or Codex's `~/.codex/hooks.json`, no longer
  fails the agent's connection. The agent stays connected, `.mcp.json` is
  listed with the files setup changed, and the hooks are listed as skipped
  with the file and the reason. The summary gives the `graphicle connect`
  command that adds them later from a normal terminal. Run by hand,
  `graphicle connect --hooks` still stops on such a file, and now names it.
  In a repository whose whole `.git` folder is read-only, as in Codex's
  default sandbox, setup also finishes: it leaves `.git/info/exclude` as it
  is, and the summary gives the lines to add to it to keep setup's backups
  and the settings file it created out of `git status`. When Codex's
  `config.toml` or Graphicle's data folder cannot be written, the error
  names it.
- An issue whose title names a version (a level-1 heading such as "...
  Version Check Preferences") lost every section under its title to the
  environment-section rule, so the question came out empty and `graphicle
  pack` refused it. A title or a heading that merely names a version now
  stays, and a question that still comes out empty is ranked as written.
  This was found before the release, in 18 of 633 issues of a public
  software engineering benchmark.
- Dependencies: zstd 0.14.3, zstd-safe 8.2.0 and zstd-sys 2.1.2, for
  RUSTSEC-2026-0391, -0398, -0403 and -0410. zstd 0.13.3 and zstd-safe 7.3.0
  stay: only jieba-rs 0.11.0 brings them in, through include-flate 0.3.4, to
  unpack its built-in dictionaries, a path that does not reach the affected
  functions, and each `deny.toml` says so for those three advisories.
- `graphicle config set model ollama` reads `OLLAMA_HOST` when its scheme
  is not lowercase (`HTTP://gpu.lan:11434`); it saved
  `http://127.0.0.1:11434` instead (#12, by @rudycelekli).
- `graphicle serve --port 80` accepts the `Host` header that browsers and
  curl send without the default port (`127.0.0.1` or `localhost`); it
  refused every such request as an unexpected host (#13, by @rudycelekli).
- A memory library test failed now and then on macOS when tests ran in
  parallel: two tests could make scratch folders of the same name, since
  macOS clocks tick in microseconds. A per-process counter keeps them apart.

### Docs

- New pages:
  [Packs](https://github.com/Graphicle-AI/graphicle/blob/v0.1.9/docs/pack.md),
  [Fix](https://github.com/Graphicle-AI/graphicle/blob/v0.1.9/docs/fix.md),
  [Review](https://github.com/Graphicle-AI/graphicle/blob/v0.1.9/docs/review.md),
  [See it, find it, fix it, check it](https://github.com/Graphicle-AI/graphicle/blob/v0.1.9/docs/flow.md),
  [Muse Code](https://github.com/Graphicle-AI/graphicle/blob/v0.1.9/docs/muse-code.md),
  and
  [the whole-repository benchmark](https://github.com/Graphicle-AI/graphicle/blob/v0.1.9/docs/benchmarks/whole-repo.md):
  its questions, arms and scorer, fixed before any model run; the context
  recall of the pack without a model (a file counts when the pack holds any
  of its text); and the cost of a question with the
  prompt cache, computed at list prices and not billed. Its model runs have
  not been made yet.
- `docs/configuration.md` covers Muse Spark, prices, the spend log and the
  caps; `docs/agents.md` covers Muse Code, and Claude Code and Codex on
  Meta's models; `docs/mcp.md` covers the `pack` tool.
- The README gives "Agent memory, compared" near the top, a still of
  0.1.8's `graphicle setup` as its hero, the 0.1.8 chart of time per
  question on kubernetes, and the new commands, and `docs/benchmarks.md`
  gives the early pre-release memory runs on GPT models.
- The benchmark kits are in the repository: `bench/whole-repo` (the
  whole-repository benchmark), `bench/pixel` (pixel to code) and
  `bench/muse-code` (Muse Code with and without the plugin, and the
  two-session memory demo).

### Packaging

- `server.json` at the repository's root is the MCP Registry entry of
  0.1.8, the latest listed release (it named 0.1.7). A release's entry is
  committed after its listing, as
  [packaging/mcpb/README.md](https://github.com/Graphicle-AI/graphicle/blob/v0.1.9/packaging/mcpb/README.md#steps-at-release-time)
  describes.
- `packaging/muse-code-plugin/` holds the Muse Code plugin as `graphicle
  connect muse --hooks` writes it, and `packaging/github-action/` the
  GitHub Action of `graphicle review`, whose scripts CI tests with
  stand-ins.

## 0.1.8 - 2026-10-26

Every crate is at 0.1.8, and `graphicle --version` prints
`graphicle 0.1.8 (core)`. The Python and TypeScript SDKs are at 0.1.8 too.

### After upgrading

- Run `graphicle sync` once in each project. Until that sync, a question
  reads the code graph from the store, which on a shallow clone of
  kubernetes took a median of 22.5 seconds per question (16 cores of a
  Linux server, the 22 questions of #8), and on a project of 2,000 files
  or more the prompt hook gives only the files that define the code names
  a prompt gives.

### Questions on large projects

- A question no longer decodes the whole saved code graph (#8). After each
  code graph build, a sync also saves a query file beside the saved graph
  (`gf-cache/<project>/query/<episode>.query`). `graphicle context`, the
  prompt hook and the memory server's `recall_project` read it in place
  (mapped into memory on macOS and Linux, read whole on Windows) and decode
  only what the question needs: every node's text once, to weigh the
  question's words, then the postings of its own words, the scoring
  strings of the nodes those leave and the records of the code it cites.
  A lookup ("Where is X defined?", a bare name, "who calls X") reads the
  symbols and references of its names. Packets and the prompt hook's
  output are unchanged, byte for byte.
  - Measured on a shallow clone of kubernetes at `ca8a2a9e4` (30,315
    files) on a Linux server, against 0.1.7 on the same cores, each call a
    new process as the hook runs it. Over the 22 questions of #8 the
    median `graphicle context` call took 1.34 seconds on 16 cores, against
    2.99 seconds with 0.1.7 (90th percentile 1.87 against 3.37 seconds),
    and 1.38 seconds on 4 cores, against 3.89 seconds (90th percentile
    1.87 against 4.56 seconds). On the 20 of them the prompt hook answers,
    it took a median of 1.53 seconds on 16 cores and 1.59 seconds on 4,
    against 3.11 and 3.96 seconds.
  - Over 8 lookups the median `graphicle context` call took 0.96 seconds
    on 16 cores and 0.99 seconds on 4, against 2.52 and 3.44 seconds.
  - With the data folder and the program dropped from the operating
    system's file cache before each call, the median over the 22 questions
    was 1.84 seconds on 16 cores and 1.90 seconds on 4, against 3.68 and
    4.55 seconds.
  - The peak memory of a `graphicle context` call fell from 3.5 GB to 0.6
    GB (the median over the 22 questions on 16 cores).
  - A question read about a third of the query file (a median of 35% of
    its pages over the 22 questions, at most 46%); a lookup read a median
    of 0.3%.
  - The query file takes 206 MB of the data folder for that clone, beside
    the saved graph's 270 MB (6.45 GB in all, against 6.24 GB).
  - The main figures of this run, with their labels, are in a table in
    [docs/benchmarks.md](https://github.com/Graphicle-AI/graphicle/blob/v0.1.8/docs/benchmarks.md#our-own-run-on-kubernetes-017-and-018).
- A sync that builds the code graph also writes the query file: on that
  clone a sync after a one-file change took a median of 117.7 seconds,
  against 109.5 seconds with 0.1.7, of which saving the graph and the
  query file took 28.0 seconds, against 21.0 seconds for the graph alone.
  A sync with nothing to do took 2.4 seconds, against 2.5.
- Going back to the head set synced just before (a branch switched back
  to) finds that head set's saved files: on that clone such a sync took a
  median of 17.2 seconds, against 28.6 seconds with 0.1.7, and a question
  then took a median of 1.38 seconds over the 22 questions. 0.1.7 kept the
  saved graph of the latest episode only, so after such a switch its
  questions read the code graph from the store, a median of 22.3 seconds
  each, until the next code graph build.
- The keys of the saved graph and of the query file now cover the
  program's sources and its locked dependencies, not only the engine's, so
  a file another build wrote is never read. The first sync after upgrading
  writes both again (26.9 seconds on that clone). Until then a question
  reads the code graph from the store (a median of 22.5 seconds on 16
  cores over the 22 questions), on a project of 2,000 files or more the
  prompt hook gives the files that define the code names a prompt gives,
  and `graphicle doctor` says so.
- A query file that cannot be written for a reason that can pass, such as
  a full disk, is tried again by later syncs after a wait (10 minutes,
  doubled after each failure in a row, at most 6 hours) and at once by
  `graphicle sync`, `graphicle setup` and `graphicle doctor --fix`. A code
  graph the file cannot hold (a node id that is not a string, a value
  nested more than 64 levels deep, a section past 4 GiB) is not tried
  again until the code graph or the program changes. `graphicle doctor`
  says which, under Questions, and what to do.
- A query file, saved graph or file relationships file that is truncated,
  malformed or damaged is read around, with the same packet, and written
  again. The query file has checksums over its header and each page, and
  the saved graph and the file relationships now end with their length
  and a checksum: a sync checks the query file's header and the other
  files' lengths, and the read that finds damage removes the file, so the
  next sync writes it. A file relationships file whose pairs name files
  outside its table no longer makes `graphicle context` fail, as it did
  in 0.1.7. `GRAPHICLE_QUERY_FILE=0` makes every question read the saved
  graph.

### Packaging

- `server.json` at the repository's root is the MCP Registry entry of
  0.1.7, the latest listed release (it named 0.1.6). A release's entry is
  committed after its listing, as
  [packaging/mcpb/README.md](https://github.com/Graphicle-AI/graphicle/blob/v0.1.8/packaging/mcpb/README.md#steps-at-release-time)
  describes.

## 0.1.7 - 2026-10-25

Every crate is at 0.1.7, and `graphicle --version` prints
`graphicle 0.1.7 (core)`. The Python and TypeScript SDKs, at 0.1.0 until
now, are at 0.1.7 too.

### Code graph

- The engine follows Graphify 0.9.95 (0.1.6 followed 0.9.91): the four
  releases 0.9.92 to 0.9.95 are ported. Against 0.9.95 every comparison check
  passes on Linux, and the file ledger marks all 96 application modules
  verified
  ([docs/benchmarks.md](https://github.com/Graphicle-AI/graphicle/blob/v0.1.7/docs/benchmarks.md#parity-with-graphify)).
  What changes in the graph:
  - YAML: `.yaml` and `.yml` files are code, read structurally: a node per
    key under its file, with `contains` edges, keys nested at most seven
    levels deep and 500 keys a file. YAML that holds data rather than
    configuration is declined by design and listed as skipped, and unsafe
    tags are refused.
  - C#: each overload of a method is a node of its own (the first keeps its
    id, the others get ids salted by their signature), and calls bind by
    argument count. A `delegate` is a type.
  - Type aliases and declarations: Python `TypeAlias`, `type X =` and
    `NewType`; Go `type A = B`; Rust `type` aliases and `union` items; C++
    namespace-scope `using A = B;` and `typedef`; Kotlin `typealias`; Dart
    `typedef`; Swift `macro` declarations and `#macro(...)` calls.
  - Names that differ only by `_`, `$`, `#` or case keep separate nodes:
    JavaScript and TypeScript members, Python module-level classes beside a
    function or class of the same id, Go and Rust declarations.
  - Calls: TypeScript `this.field` is typed from an assignment in any method;
    a JavaScript call on an untyped receiver waits for receiver-typed
    resolution; a CommonJS `require` binding no longer shadows the export it
    requires; a pytest fixture parameter is typed from the fixture's return;
    Python `module.func()` binds the child named exactly `func`; a bare call
    resolved only through a module import stays `INFERRED`; Solidity
    inherited calls, `super` calls and modifiers resolve through C3
    linearization; a Rust `use` rooted at a sibling `mod` reaches its
    declaration; an Objective-C member call resolves across a header and an
    implementation in different folders; a cross-repository call edge points
    at the call site.
  - SQL: T-SQL names are unbracketed (`dbo.Alpha`, not `[dbo].[Alpha]`),
    procedures record `reads_from` and `writes_to`, `GO` ends a statement, a
    table referenced before its `CREATE TABLE` folds onto it, and a
    `CREATE TABLE` the grammar cannot parse is recovered.
  - Each `package.json` dependency entry declares the node its importers
    resolve to, labelled `name@range`. Markdown vault-relative wikilinks
    resolve, and a bare method link needs its class cited. Objective-C
    `NS_CLOSED_ENUM` is an enum, and category ids are relative to the
    repository. Metal and CUDA qualifiers no longer create nodes without a
    source, and a `.h` with `extern "C"` goes to the C++ extractor.
  - Graph building: string confidences that drifted from the enum are
    folded onto it, on nodes as on edges; a node with a source file wins
    over a namesake without one; a build without clustering resolves imports
    through the old-stem alias index. An incremental build no longer queues
    again a file declined by design.
- The AST cache format changed, so the first sync after upgrading re-extracts
  every file once.
- graph.json keeps parallel edges between two nodes (`multigraph: true`).
  Every reader of a saved graph.json, not only the MCP server (`export` and
  `merge-driver` among them), reads a file written before (parallel links
  under `multigraph: false`) the same way, so a result does not depend on
  which release wrote the file.
- A dependency entry of a JSON manifest such as `package.json`, which the
  engine now labels `name@range` and joins to the package it declares, keeps
  the stable id of its name (its label up to the first `@` after the first
  character), so a reviewed memory linked to it survives the new label and
  later version bumps. Other keys that hold `@`, such as pnpm overrides or a
  jest `moduleNameMapper` pattern, keep their whole label.
- A code graph episode is named by the engine's graph format as well as the
  project's files, so the first sync after upgrading builds the graph again
  even when no file changed: C# overloads become nodes of their own and the
  nodes the new engine no longer makes are closed. Before, an unchanged
  project kept the older engine's graph until one of its files changed.
- `graphicle graph update` works on a project whose graph is over 512 MiB,
  such as kubernetes: before, even its first update failed, reading back its
  own temporary graph under the size cap. The engine reads a graph.json over
  the cap that it wrote itself (`update`, `watch`, `serve`, `query`,
  `explain`, `path`, `merge-graphs`, `cluster-only`, `label`); a larger
  graph.json from elsewhere is still refused, and `GRAPHICLE_MAX_GRAPH_BYTES`,
  when set, applies to every file. A graph.json over the cap that an earlier
  release wrote has no record of being the engine's own, so updates and hook
  rebuilds refuse it until it is rebuilt once, as the error says: run
  `GRAPHICLE_MAX_GRAPH_BYTES=<N>GB graphicle graph extract --force <project>`,
  with N above the file's size (add `--code-only` when no LLM backend is set
  up).
- The engine extracts YAML structurally (above), so graph.json and the
  `graphicle graph` commands have its keys, but Graphicle Core does not yet
  use those keys for its answers: its code graph leaves out the YAML files
  the engine reads as keys (the `apm.yml` package manifests stay in, as
  before), and a YAML path quoted in a memory is taken for data, as a JSON
  path is, not for a code file.

### Commands, hooks and the language model

- `GRAPHICLE_BACKEND` chooses the LLM provider (`none` turns LLM work off);
  an auto-detected provider is named before anything is sent.
- The semantic extraction prompt now pins each node to the file it came
  from; cached semantic results are extracted again once, which costs one
  round of LLM calls.
- Git hooks: a rebase, `pull --rebase`, cherry-pick or revert rebuilds the
  graph once; a merge commit rebuilds; a new branch at the same commit does
  not; hooks run from the repository's top level. Run
  `graphicle-graph hook install` to add the post-rewrite hook and update the
  others.
- `--version` and `--help` no longer refresh installed skills, and `--help`
  lists every `--platform` value.
- `query` refuses a name shared by several nodes (exit 3) and an unknown
  `--seed` exits 4; `explain`, `get_node` and `get_neighbors` report a
  substring near miss instead of answering with it.
- Merged graphs: `explain` and `path` keep same-path files of two repos
  apart and name the repo; the tree export draws one root per repo.
- `prs` reads CI state from legacy status contexts and keeps a default
  branch such as `release/2.x` whole; a Maven pom contributes only its own
  dependencies; `serve` over streamable-http on an IPv6 address accepts its
  `[::1]:port` Host header.
- Detection: only a valid `.git` marks a repository root; versioned shebangs
  such as `python3.12` are recognised; an unreadable PDF warns once; images,
  audio and video count no words; a `.graphiclerc` saved with a byte order
  mark keeps its first setting.

### Connections

- The `graphicle` program allows plain `http://` to every loopback address
  for the services it calls (models, connectors, OAuth, vector and graph
  endpoints): the rest of 127.0.0.0/8, such as `127.0.0.2`, and IPv4-mapped
  loopback, such as `[::ffff:127.0.0.1]`, as well as `localhost`,
  `127.0.0.1` and `[::1]`. Before, a local model or connector on another
  loopback address was refused (#11, by @rudycelekli).

### SDKs

- The Python SDK keeps the service's `Retry-After` on an error:
  `GraphicleError.retry_after` is the delay in seconds (from a number of
  seconds or an HTTP date), and the error's text names it, so a caller whose
  monthly request budget is spent knows when to try again, as the
  TypeScript SDK's `retryAfterSeconds` already says (#10, by @rudycelekli).
  A `Retry-After` that is neither leaves `retry_after` as `None`, and the
  error is still a `GraphicleError`.

### Security

- URLs with credentials, a bad or zero port, or no host are refused;
  multicast addresses are blocked; Obsidian tags and Google Workspace
  sidecars escape hostile values.
- Installing or refreshing a skill (`graphicle graph install`) no longer
  writes through a symbolic link. A link at the skill's `SKILL.md`, or at
  the version stamp beside it, is replaced by the new file, and the file it
  pointed to is left alone; before, that file was overwritten.
  `references/` is staged in a new private folder with a random name
  instead of `references.tmp`, so a link put there in advance cannot
  redirect the copy, and the backups (`SKILL.md.bak` and the
  `.graphicle-bak` copies of agent settings) never follow a link.
- A symbolic link that a repository commits at `.graphicle/.rebuild.lock`,
  at the rebuild's temporary graph or at `.gitattributes` is no longer
  followed: before, `graphicle graph update`, a hook rebuild or
  `graphicle graph hook install` could truncate, overwrite or add a line to
  the file it names. `update --no-cluster` and the GraphML export replace a
  link at their output instead of writing through it.
- Credential-shaped text in docstrings and comments is redacted before it
  becomes a rationale label.

### Dependencies and packaging

- yoke-derive 0.8.4 replaces 0.8.3, which its authors yanked and
  `cargo deny` warned about. Only the lock files and the third-party
  licence pages change.
- `server.json` at the repository's root is again the MCP Registry entry of
  the latest listed release (0.1.6; it still named 0.1.2). An entry lists
  the SHA-256 of each bundle the `mcp-registry` workflow builds from the
  release, so a release's entry is committed after its listing, as
  [packaging/mcpb/README.md](https://github.com/Graphicle-AI/graphicle/blob/v0.1.7/packaging/mcpb/README.md#steps-at-release-time)
  describes.

## 0.1.6 - 2026-10-24

Every crate is at 0.1.6, and `graphicle --version` prints
`graphicle 0.1.6 (core)`.

### SDKs

- The Python and TypeScript SDKs allow plain `http://` to every loopback
  address: the rest of 127.0.0.0/8, such as `127.0.0.2`, and IPv4-mapped
  loopback, such as `[::ffff:127.0.0.1]`, as well as `127.0.0.1`, `[::1]`
  and `localhost`. Before, a local service on another loopback address
  needed `insecure_http=True` (Python) or `insecureHttp: true`
  (TypeScript) (#9, by @rudycelekli). The Python SDK reads an IPv4-mapped
  address by its IPv4 address on every Python version, and its requests
  to all of these addresses ignore proxy settings, as they did for
  `127.0.0.1`.

### Security

- tokio 1.53.3 and ctutils 0.4.4, for RUSTSEC-2026-0386 (the safe
  constructors of tokio's `AsyncFd` break I/O safety; Graphicle does not
  call them) and RUSTSEC-2026-0385 (ctutils made masks and `u128`
  comparisons that were not constant-time). Only the lock files and the
  third-party licence pages change.
- The Python environment of the engine's comparison suite pins pypdf
  6.21.2, which fixes security advisories.

### Documentation

- `docs/benchmarks.md` has a section of
  [independent measurements](https://github.com/Graphicle-AI/graphicle/blob/v0.1.6/docs/benchmarks.md#independent-measurements).
  Its first entry is the rerun by @selimaytac, who reported #8, of their
  22 questions about kubernetes on an Apple M3 Pro, with no answer model:
  `graphicle context` took a median of 153.5 s per question on 0.1.3 and
  9.5 s on 0.1.5, and the prompt hook returned a packet for 18 of 22.

## 0.1.5 - 2026-10-22

Every crate is at 0.1.5, and `graphicle --version` prints
`graphicle 0.1.5 (core)`.

### Questions on large projects

- Each sync keeps a passage index of the project's files in the store, and
  `graphicle context`, the memory server's `recall_project` and the prompt
  hook look a question up in it instead of reading and scoring every file of
  the project (#8, reported by @selimaytac). A sync reads only the files
  that changed since the last one. After each code graph build, Graphicle
  also saves the graph and its search index in the data folder
  (`gf-cache/<project>/query/`), so a question reads them instead of
  rebuilding them from the store.
  - Over the 22 kubernetes questions of #8, on a shallow clone of
    kubernetes (30,315 files) on 16 cores of a Linux server, a question to
    `graphicle context` took a median of 3.1 seconds, against 27.9 seconds
    with 0.1.4. On duckdb (14,309 files) the median over 10 questions was
    2.6 seconds, against 20.9 seconds.
  - A question ranks every passage of the project, as the full path does
    within its bounds. A project past those bounds (50,000 documents or
    40,000 passages) was ranked on the documents that matched the question
    best, and is now ranked whole.
  - The index takes room in the store and time at a first sync: on that
    kubernetes clone the data folder held 6.22 GB after a first sync,
    against 5.80 GB with 0.1.4, and the sync took 239.5 seconds, against
    235.4 seconds. On duckdb a first sync took 125.9 seconds, against 112.4
    seconds. Later syncs read only the files that changed; a sync merges
    the index when the files that changed since it was built hold more than
    30% of its passages, and `storage.prune` merges it too, so the entries
    of files that went away go.
  - A store from an earlier version builds its index at its next sync, and
    its questions take the full path until then. Calls with options the
    index does not serve, such as `include`, and projects with reviewed
    memory also take the full path; `docs/configuration.md` lists them. The
    packet says what it read in `passage_index`.
    `GRAPHICLE_PASSAGE_INDEX=0` makes every question take the full path,
    and `GRAPHICLE_SAVED_GRAPH=0` makes it read the code graph from the
    store.

### Prompt hook

- The hook's time budget is a setting: `graphicle config set
  prompt_context.seconds N` (1 to 25 seconds, 10 by default) or the
  `GRAPHICLE_PROMPT_CONTEXT_SECONDS` variable. The hook's own timeout is 30
  seconds.
- While the passage index of a project of 2,000 files or more is not
  ready, the hook gives the files that define the code names a prompt
  gives, from the code graph, without reading the project's other files. A
  smaller project, and a project with reviewed memory (whose memories only
  the full read gives), are asked in full, as before.
- When the hook gives up for time, or answers that way, Claude Code shows
  one note a session that says so and what to run. For a project with
  reviewed memory, the note says that a sync does not make it faster.
- Graphicle 0.1.4 and earlier refuse a configuration that sets
  `prompt_context`, a field they do not know. After setting it, do not run
  an older Graphicle on the same data folder, such as an MCP server started
  before the upgrade.
- A prompt that asks where code is in plain words, such as "Which file and
  functions compute this?" or "Where is that latency recorded?", is read as
  a code question.
- Over the same 22 kubernetes questions, the hook gave a packet within its
  10-second budget for 20 of them, and read the other 2 as questions in
  plain words. With 0.1.4 it gave none: it ran out of time on 11 and did
  not read the other 11 as code questions.

### Setup and doctor

- `graphicle doctor` has a Questions check: whether the project's passage
  index is built and current and its code graph saved, or what makes its
  questions slow (a missing or stale index, reviewed memory), with the
  command that fixes it. `graphicle setup` says the same in its `questions`
  field and line. A project of fewer than 2,000 files is only described.

### Packets

- A packet tells its reader to read on with what the reader has:
  `read_project_source` over MCP, and from `graphicle context` and the
  prompt hook, the line of the file to continue at. It no longer names
  `read_repository_file` or `read_project_evidence`, which no tool set has.

### Sync

- On a file system that ignores case (the default on macOS and Windows), a
  tracked file that an ignore pattern matches only in another case, such as
  `src/Foo.js` against `foo.js`, was read twice, and its bytes counted
  twice. It is read once. Two hard links to one file are still read as two
  files.

### Extension interface

- When an installed extension sets the most tokens a read's result may
  hold, a `context` result over it is packed again with a smaller budget
  until it fits (at most 4 rounds, down to 256 tokens, reported in
  `response_sized`), instead of refused with 413. Other reads are still
  refused.

### Windows

- `graphicle.exe` and `graphicle-graph.exe` are signed through SignPath with
  a test certificate, which Windows does not trust, so Windows still treats
  them as unsigned programs. When the signing fails, the release ships them
  unsigned; the release notes say which.
- `graphicle.exe` and `graphicle-graph.exe` carry version information
  (product name, company, description and version), which Windows shows in
  each file's properties.

### Other changes

- The install scripts and the Homebrew formula say that Graphicle checks for
  a newer release at most once a day by default, and how to turn that off.
- The code signing policy, `docs/code-signing.md`, lists every case in which
  Graphicle contacts the network, what each one sends and how to turn it off
  or avoid it.
- The release workflow's signing and Homebrew tap jobs run in GitHub
  environments that only version tags reach and a maintainer approves, and
  only those jobs can read their tokens.

## 0.1.4 - 2026-10-21

Every crate is at 0.1.4, and `graphicle --version` prints
`graphicle 0.1.4 (core)`.

### Connections

- HTTPS connections trust the certificate authorities of the operating
  system as well as the roots bundled with Graphicle: the Windows
  certificate store, the macOS keychains and the system bundle on Linux. A
  model provider whose certificate comes from a company's own certificate
  authority is now reached once that authority is in the system trust
  store, as curl and browsers reach it. Before, only the bundled roots were
  trusted, so such a provider could not be reached (#7, reported by
  @Mohammed-Tabiza). When `SSL_CERT_FILE` or `SSL_CERT_DIR` is set, its
  certificates are read instead of the system's. If the system's
  certificates cannot be used, Graphicle still connects with the bundled
  roots.
- A failed connection to a model provider now gives its cause, for example
  `invalid peer certificate: UnknownIssuer`. When the server's certificate
  is refused, the error also says what to do: install the certificate
  authority in the system trust store, or set `SSL_CERT_FILE` to a PEM file
  that holds it. `graphicle ask` reports that the provider's certificate is
  not trusted, instead of suggesting a network or proxy problem. The graph
  engine's model, pull request triage and URL fetch requests give the same
  causes and hint.

### Other changes

- The Python environments of the engine's comparison suite pin Pillow
  13.0.0 and certifi 2026.10.19, which fix security advisories.

## 0.1.3 - 2026-10-21

Every crate is at 0.1.3, and `graphicle --version` prints
`graphicle 0.1.3 (core)`.

### Code graph

- Markdown front matter that opens a bracketed list or mapping on a key's
  line (`key: [` or `key: {`) and continues it on lines indented no further
  than the key, such as a closing `]` at the start of a line, is now read as
  YAML. Graphify's YAML parser (PyYAML) ignores indentation inside brackets,
  but Graphicle's rejected such lines, so the page's stored front matter fell
  back to plain text: its fields became strings and the list's entries were
  lost. An `apm.yml` manifest written this way, which the same parser reads,
  gave a manifest parse error and is now read too. This was the cause of the
  known difference in the graph after `graphicle graph update` on
  gohugoio/hugo.
- Linking imports across a workspace's projects is incremental. After a sync,
  a project's imports are linked again only when the project changed, when a
  project it can import from changed what it exports, or when the set of
  linked projects changed. The other projects keep their links, and their
  graphs are not loaded again: each project's export index is kept with the
  links. The links recorded are the same as those of a full pass. The
  `graph_link` result lists the projects linked again (`relinked`) and those
  whose links were kept (`reused`).

### MCP Registry

- Each MCP bundle holds the Graphicle icon, which Claude Desktop shows beside
  the extension, and the registry entry links to the same icon
  (`packaging/mcpb/icon.png`). The workflow builds a release's bundles with
  the scripts on main, so the bundles of 0.1.2 have the icon too.

### Agents

- Agents are told when Graphicle helps and when grep is enough, in the same
  words wherever they read it: use `recall_project` or `query_graph` for
  where or how something works, for questions across files or history, and
  before changing unfamiliar code; for an exact identifier you already know,
  grep is enough. Answers cite file and lines.
  - Claude Code's and Codex's session-start hook prints this as one line at
    the start of every session in a project (Claude Code still skips a
    resumed conversation), before the reviewed memory, open work and stale
    memory it printed already. A project with nothing reviewed, open or
    stale used to get no session-start text.
  - The server's instructions start with it, in the essentials set and in
    the whole server.
  - The descriptions of `recall_project`, `query_graph`, `get_node`,
    `get_neighbors` and `shortest_path` end with one sentence on when to use
    the tool instead of grep. Apart from these sentences, the whole server's
    instructions and tools (`--tools all`, which the other agents get) are
    0.1.0's.
  - The Graphicle section that setup writes in `AGENTS.md` and `CLAUDE.md`
    says it. Running setup again brings an existing section up to date, and
    `graphicle disconnect` removes it as before. `setup --tools all` still
    writes 0.1.0's section.
  - The opt-in guard hook's reminder before a search says it too.
- `graphicle setup --prompt-context` (off by default) adds a Claude Code
  UserPromptSubmit hook: before each prompt that is a code question (the
  classifier `context` uses), it runs `graphicle context` and gives the
  packet's Markdown, at most 8,400 characters, as the prompt's additional
  context. Short prompts, questions in plain words, folders of no project,
  failures and answers slower than 10 seconds add nothing; the prompt is
  never refused, and the hook waits up to about 10 seconds. The prompt
  reaches `graphicle context` on its standard input (`graphicle context -`
  reads the question there). `graphicle disconnect` removes the hook.

### Context and recall

- `graphicle context` prints at most 8,400 characters by default (new
  `--max-chars`; it was 4 characters per `--max-tokens` token, 12,000 by
  default), so a prompt hook that prints the packet under a header stays
  below the 10,000 characters Claude Code shows in full. With only
  `--max-tokens`, the packet still holds 4 characters per token.
- A `notes_first` option, off by default: `notes_first: true` in `context`,
  `graphicle call context`, the SDKs and answers, `graphicle context
  --notes-first`, or over MCP beside `recall_project`'s arguments (and
  `graphicle mcp --set notes_first=true` for a server). Every packet's
  `notes_first` says what applied. It turns on three changes:
  - A question that names no code (no backtick span, `snake_case` or
    `camelCase` name, `Type::member`, `call()` or code file) takes the first
    50 files of the fused ranking, one passage each. Code files keep their
    places; the notes and documents among them are ordered again among the
    notes' places: a note whose name or folder matches the question's words
    (Porter stems, so close variants match), whose headings match, or with a
    newer date (the latest ISO date, up to today, in its path or text) comes
    first. A question that names code keeps the code ranking.
  - A packet quotes one window per file, and one of files with identical
    bytes; the next files take the freed places (`notes_first.one_per_file`
    counts what was dropped). `include_tests: true`, which `--tools all`,
    `graph` and `memory` send for `recall_project`, and an explicit
    `origin_quota` keep the windows as ranked.
  - Excerpts say how old they are. Each excerpt of a project file cites the
    date its file last changed (`(changed 2026-10-15 05:10 UTC)`: the last
    commit when Git tracks the file unchanged, else its modification time).
    An excerpt that names code (a code-shaped document link) whose file
    changed later says `may be stale: <file> (defines <name>) changed
    <date>`. Excerpts are not compared with one another, and dates do not
    reorder them. Imported notes and documents carry no review state, so the
    stale flags of reviewed memory never applied to them. `source_age: true`
    or `false` sets the dates alone.
- An identifier route for `context`: a question that is an identifier, or
  names identifiers and only asks where they are defined or used
  (`` `parse_config` ``, `where is Store::open defined`, `who calls
  ParseConfig`), gets each name's definitions and references from the code
  graph as `path:line` lines within the call's budget, and one line with the
  titles of the reviewed memories linked to them, instead of the full
  retrieval's excerpts (the packet's `evidence` is empty). `recall_project`
  in the essentials set and `graphicle context` take it unless told
  otherwise (`identifier_route: false`, `graphicle context
  --no-identifier-route`, `graphicle mcp --set identifier_route=false`); the
  HTTP API, `graphicle call context`, the SDKs, the app and `--tools all`,
  `graph` and `memory` take it only when a call sets `identifier_route:
  true`. A name is a backtick span, a `snake_case` word, a camelCase or
  PascalCase word with a capital inside it, a dotted or `::` path, or a
  call `name()`. A name alone takes the route; other words may only ask
  where the names are or what uses them, and one of them must. A question
  in plain words, one that asks for more (what a name is, its code or
  members, what it calls, where it is mentioned), a call with `needs`, and
  a name the code graph does not define keep the retrieval. Every project
  packet's `route` says which path ran, and why the retrieval did.
- Code questions rank source files first (`code_first`, `"auto"` by
  default). A question that names code (an identifier or a code file) or
  reads like an issue or bug report (a code block, a stack trace or error
  line, or two report words such as crash, error, fails, expected) counts
  the passages of documentation, test and vendored or generated files at a
  quarter of their fused score, in the ranking itself, so the excerpts and
  the budget go to source files first; the code-graph arm leaves Markdown
  nodes out for such a question. Tests are recognised by folder and name in
  Rust, Python, Go, Java, Kotlin, C#, JavaScript, TypeScript, C, C++ and
  Ruby. A question about documents, notes or decisions that is not a report
  keeps 0.1.2's order, and test files keep their weight when a question
  that is not a report names a test or the call sets `include_tests: true`.
  A test or document that defines a code name the question names (with an
  underscore or an inner capital, such as a test helper or a fixture) keeps
  its weight when no source file defines that name.
  A camelCase name needs two lowercase letters before its capital, digits
  between allowed (`getChoices`, `utf8Decoder`; not `iPhone` or `eBay`);
  a PascalCase name counts when it has a capital inside it, as for the
  identifier route (`IsNull`, `ParseConfig`).
  `code_first: false` keeps 0.1.2's order for any question, `true` ranks
  source first for any question. The packet's `code_first` says what
  applied and how many files of each kind it lowered.
- A code question's query is its content, not its instructions
  (`query_focus`, `"auto"` by default): when the question marks its content
  (tags of its own such as `<issue>` or `<pr_description>`, opening at the
  start of a line, or `--- BEGIN ISSUE ---` / `--- END ISSUE ---` lines),
  the marked text with every word, followed by the names of code in the
  prose outside (placeholders such as `ClassName.method_name` and fenced
  examples left out); without marks, the question without general words of
  instructions (find, fix, change, list, file, function, please, ...).
  Either way a code host's links to files (`.../blob/<ref>/src/x.rs#L10`)
  become the files' paths, and other web addresses and commit hashes are
  left out. BM25, the code-graph arm, the choice of files past the passage
  bound, a local reranker and the excerpts' focus read it; the packet keeps
  the question, and its `query_focus` says what was marked and left out. A
  prompt that wraps an issue in instructions used to rank by the
  instructions' words as much as the issue's. `query_focus: false` reads the
  whole question.
- The identifiers and file paths a code question names rank their files
  (`identifier_arm`, `"auto"` by default). The identifier route kept the
  retrieval for a question that asks more than where names are, and dropped
  the names it had found; now a code question's identifiers (from code
  blocks and spans: every identifier; from prose: `snake_case`, camelCase,
  calls, qualified names) are matched as whole words, case-sensitively, in
  the project's passages, and found at their definitions in the code graph.
  Each file scores the weights of the names it holds (rarer names weigh
  more, a definition counts twice) plus a bonus when the question names its
  path, and its two best passages join the fusion as the arm `identifiers`
  (`arm_weights` key `identifiers`). The packet's `identifier_arm` lists the
  names and paths used; the route's report says `names_fused`.
  `identifier_arm: false` leaves the arm out.
- After its excerpts, a code question's packet lists the paths of the next
  10 ranked files that no excerpt quotes, one line each (`more_files`, 0 to
  40; left out, 10 for a code question and 0 for any other). The room is
  kept from the excerpts' share, at most an eighth of the budget, so the
  packet stays within `max_chars` and `max_tokens`; at `graphicle
  context`'s 8,400 characters the excerpts quote three or four files, and
  the files after them used to be cut. The packet's `more_files` lists
  them, and `graphicle context` prints them under "Next ranked files".
- `code_first`, `query_focus`, `identifier_arm` and `more_files` are flags
  of `graphicle context` (`--code-first`, `--query-focus`,
  `--identifier-arm`: auto, true or false; `--more-files N`) and switches
  of `recall_project` over MCP that `graphicle mcp --set` can fix; packets
  echo them in `mechanisms.switches`.
- A `context` question over 16,000 bytes is shortened instead of refused:
  to its tagged content when that fits, else to its beginning, cut at a
  line end, followed by the identifiers and file paths of the part cut
  (within 3,000 bytes). The packet's `question_shortened` gives the
  question's size, what was kept and how many names were added. A long
  issue pasted into a prompt hook used to get no packet.
- Past the per-question bounds (50,000 documents or 40,000 passages), the
  files a question reads are chosen by a BM25 score of its words in each
  file (rarer words weigh more, a word's count is divided by the file's
  length, a word in the path counts twice more), among at most 64 words,
  those written as code first. The count of the first 24 words a file
  holds chose them before, so large files filled the bound.

### Memory, recall and answers

- `get_node` lists, for its symbol, the current reviewed memories anchored to
  it and the documents the reader may read that name it (`linked` in its
  structured answer, and a line in its citations text). Stale, draft and
  retired memories are never listed. A connection to one project lists that
  project's only, also when `repo` reads another project's code graph.
- Recall and answers take a `symbol_links` switch for the links between
  reviewed memory and code symbols. Left out, a packet counts the links of
  its excerpts and memories (`symbol_links`, status `counted`) and follows
  none. `symbol_links: true` follows them one step each way: an excerpt of
  code brings in the current reviewed memories linked to a symbol inside it,
  and a reviewed memory brings in the current code of the symbols it is
  linked to. `symbol_links: false` reads none. `graphicle mcp --set
  symbol_links=true` (or `false`) fixes it for a server.
- The packet's `mechanisms` now fill `symbol_links_enabled`, and
  `links_used.memory` counts the memory links an answer used.
- `memory.template` exports a project's reviewed procedures and decisions as
  an engagement template. Each memory keeps only its kind, title, body and
  state: quotes, links, ids, dates and authors are left out, and the
  project's names, the files and symbols it cites, e-mail addresses, links,
  credentials and terms you list are replaced by placeholders.
- `memory.import_pack` imports reviewed memories, from an export or a
  template, into another project as drafts to review. Symbol links are
  matched to that project's code by path, qualified name, kind and ordinal,
  and follow a symbol that moved to another file. Quotes are kept only where
  that project's own sources hold them. Whatever cannot be matched is
  dropped and counted.

### MCP

- `workspace_map`, a tool of workspace connections (the `graph.map`
  operation over HTTP, the SDKs and `graphicle call`), maps the projects an
  access key can read, 8 at a time: for each project, its files, documents,
  code symbols, memories and links (imports from and into the other
  projects, document mentions linked to its code, code symbols its memories
  are anchored to), and the recorded imports between the projects, by pair.
  Each project is counted as the key's own reads of it count it, and a
  project the key may not read is left off the map.
- `recall_project` takes a `cross_repo` switch for the step of a workspace
  recall that follows imports between projects: `false` turns that step
  alone off, and `true` turns it on even with `graph_retrieval: false`. Left
  out, it follows `graph_retrieval`.
- Every recall packet echoes the switches in force under
  `mechanisms.switches`. `graphicle mcp --set` (and `serve --mcp-set`) also
  fixes `cross_repo` and `scope` (`project` or `workspace`), and refuses a
  switch it does not know.
- `graphicle mcp --manifest` prints what the server would serve, as JSON,
  and exits: the program's version, edition and sha256, the tools and
  instructions with their sha256, the switches it fixes, and the schema and
  format versions.

### Sync

- A folder sync reads the files Git tracks even when a `.gitignore`
  pattern (or `.git/info/exclude`, or the global excludes) matches them
  (`git ls-files --cached --ignored --exclude-standard`): Git versions them
  with the rest, so they are the project's sources, as in a repository that
  ignores its build output `jsonata.js` and tracks `src/jsonata.js`. Files
  Git does not track are left out by those patterns as before, and
  `node_modules`, `target`, credentials and the other skipped names stay
  out, and so does a file an `.ignore` file leaves out. The freshness check
  reads the same files, and the sync's output counts them
  (`tracked_ignored`). `graphicle config set sync.tracked_ignored false`
  leaves tracked files that a pattern matches out, as 0.1.2 did; the
  environment variable `GRAPHICLE_SYNC_TRACKED_IGNORED` overrides the
  setting. The refusals of a project or folder too large say that
  `.gitignore` and `.git/info/exclude` cover untracked files only.

### Data

- `graphicle offboard --project PROJECT` removes one project's data from the
  data folder: its sources, memory, code graph and history in the store, its
  graph cache, and its copies in the backups kept in the data folder's
  `backups/` folder. Other projects stay, in those backups too. It shows the
  plan first and changes nothing until it runs again with
  `--apply PLAN_HASH`, which carries out exactly that plan. It writes a
  hash-chained deletion receipt that lists what it removed, touches nothing
  outside the data folder, and names what it does not cover, such as backups
  written elsewhere. A path it refuses (a link, or a backup with another
  name) is never changed and may still hold the project, so the offboarding
  is then reported as not fully verified.
- Offboarding never follows a link in `backups/`, such as a `gf-cache` link.
- `graphicle backup` without a path writes the backup to the data folder's
  `backups/` folder.
- On Windows, `graphicle offboard` leaves every backup in `backups/` as it is
  and lists it as refused: Windows does not tell Graphicle whether a backup
  file has another name outside the data folder.

### Usage report

- The owner's usage report (`GET /v1/workspaces/WORKSPACE_ID/usage`) and the
  Usage view of the app now show what Graphicle served this month (`served`):
  - the tokens of every MCP tool result and MCP resource read;
  - over HTTP and through `graphicle call` and `graphicle context`, of every
    operation an MCP tool runs and of the other reads of a project's
    sources, memory, evidence or graph (such as `memory.export` and
    `document.original`);
  - of the session-start context that capture hooks give a coding agent.

  Tokens are counted with cl100k_base on the text served, by credential,
  agent session, channel, tool or operation, project and UTC day. A result
  that names no project but serves several, such as a recall over every
  project of a workspace, is counted on those projects: its tokens split by
  the bytes served from each, and the call once, on the largest share.
  Operations that return only settings, counts or listings, such as
  `list_projects`, are not counted. These are the tokens Graphicle returned,
  not your agents' bill: Graphicle does not see your agents' own model calls.
- These counts never leave your machine. Graphicle keeps counters only, in a
  new `am_served_usage` table of the local store: no question, result,
  source id or client session id.
  - Counting adds no write to a call; counts are written in batches a few
    seconds later and when the program ends.
  - Rows go with their project and are kept 25 months, and an agent
    session's rows are folded into rows without a session after 90 days. One
    access key keeps at most 200 agent sessions a UTC day; the calls of
    further sessions that day are counted under one shared session,
    `other-sessions`.
  - `deletion.state` lists the table as history.
  - The table is created the first time the new version opens an existing
    store.
- Beside them, the report lists Graphicle's own recorded model tokens by
  credential and by model (`model_usage_by_credential`,
  `model_usage_by_model`). `cost` stays null.
- The MCP endpoints over HTTP (`/mcp/WORKSPACE_ID` and
  `/mcp/WORKSPACE_ID/PROJECT_ID`) send an `Mcp-Session-Id` header with their
  answer to `initialize`.
  - The server signs the id, and the usage report uses it only to group a
    client's calls into one agent session. An id the server did not sign is
    counted under one shared session, `mcp:unverified`.
  - A client that does not send the id back is served as before.
  - Each `graphicle mcp` connection over stdio is one agent session. A
    capture hook labels its start context with a digest of the coding
    agent's session id, never the id itself.

### Store

- A store can record the oldest Graphicle that may open it, as a store format
  (a new `store_readers` table, created when the new version first opens an
  existing store). From this version on, Graphicle checks it before it
  changes anything in a store, and refuses a store that needs a newer
  program, naming the version it needs. Versions before 0.1.3 do not check.
- A store can name credentials that a program must refuse unless it
  enforces their restrictions itself: a table or view
  `credential_requirements` (`credential_id`, `capability`). Graphicle
  refuses each credential it names with a capability that `build.info` does
  not list, instead of granting the credential its role's full access, also
  in a restored portable backup. Graphicle Core lists none of these
  capabilities. Graphicle 0.1.2 and older do not read the view and grant
  such credentials their role's full access, so do not open a store that
  names credentials there with them.

### Extension interface

- `Extension::authorize` lets an installed extension refuse or narrow a
  request of a credential other than the owner capability wherever Graphicle
  authorizes one (`Access`): a permission, a repository, a route that names
  a workspace, an operation, an MCP tool. It may narrow an operation's
  arguments and set the most tokens a read's result may hold: a larger read
  result is refused with 413, and a write's result is never refused once it
  is saved. `Extension::role_permits` lets it define roles beside
  Graphicle's, and `Extension::grant_role` may refuse a new credential on a
  role it defines. The caller of a request is resolved by authentication
  alone, so these hooks see every credential, whatever its role grants; an
  error other than a refusal while resolving it fails the request.
- `Extension::served_allowed` lets an installed extension refuse a metered
  MCP tool call, MCP resource read or HTTP operation of a credential other
  than the owner capability, once `authorize` allowed it, for example for a
  quota. `Extension::metered_operations` meters an extension's own reads of
  project text.
  - A refusal with status 429 carries `Retry-After` until the next UTC month.
  - Core installs no extension, so it refuses and caps nothing, and the
    command line and capture hooks are never refused for a quota.
  - A commercial edition for teams is in private preview.
- `Extension::sources` adds connector kinds beside Graphicle's (the `Source`
  trait): their connectors are added, scheduled, synced and enforced on read
  as Graphicle's own.
- `Extension::vault_key` lets an extension hold the key that seals the
  store's vault, for example a key held by an external key service; the
  vault then never falls back to the device key file.
- `Extension::cross_site_post` names a POST route that an extension
  authenticates by itself, such as a sign-in callback that another site's
  page posts to. Such a request skips the origin and browser-mutation
  checks; the host check stays.
- `JobKind::cpu_bound()` marks an extension's job kind as CPU-bound, so a
  worker leases it only with a CPU permit, as it does Core's projections.
- Routes under `/v1/workspaces/WORKSPACE_ID/usage/` are outside the monthly
  request budget, like the usage report.

### Other changes

- rustls-webpki 0.103.16, webpki-roots 1.0.10 and webpki-root-certs 1.0.10:
  certificate paths through the distrusted emSign intermediates are refused
  (RUSTSEC-2026-0382 and RUSTSEC-2026-0381).

## 0.1.2 - 2026-10-18

Every crate is at 0.1.2, and `graphicle --version` prints
`graphicle 0.1.2 (core)`.

### Dashboard

- A light theme for the app that `graphicle open` shows, and for its product
  and documentation pages (#2). The app follows the operating system's light
  or dark setting; Settings > Appearance picks System, Light or Dark, and the
  browser remembers the choice. The theme is set before the page is first
  drawn, so the page never flashes in the other theme. In the light theme,
  text has a contrast of at least 4.5:1 with its background, and control
  borders and focus rings at least 3:1. The dark theme looks as before.

### MCP Registry

- From this release on, the `mcp-registry` workflow adds one MCP bundle
  (`.mcpb`) for each release target to the GitHub release and lists the
  release in the official MCP Registry as `io.github.Graphicle-AI/graphicle`.
  Each bundle's manifest follows the MCP bundle format (checked against its
  0.3 schema) and runs `graphicle mcp` on a project folder that
  `graphicle sync` has set up. `packaging/mcpb/README.md` describes the
  bundles and the steps at release time.

### Other changes

- The new-release check is no longer made again with the next command when
  a command ends before it does. 0.1.1 already counted a failed check as the
  day's check, but a check still running when a command ended was dropped
  and made again with the next command. Each check is now recorded before its
  request goes out, so such a check counts as the day's check, though not as
  a failure. Repeated real failures now back off: after a check that fails,
  or that GitHub refuses or limits, the next one waits a day, then two days
  after a second failure in a row, then four. The request is still one GET
  with no code or data in it, and the notice is still printed at most once a
  day.
- A test of `.graphiclerc` handling could fail when tests ran in parallel:
  another test could clear the record of folders already warned about
  between its two calls. The tests that change that process-wide state now
  run one at a time.
- Agent setup and detection read a Codex configuration whose multi-line
  strings hold non-ASCII text instead of crashing (#3, thanks
  @rudycelekli).
- Session capture cuts a conversation opening longer than 300 bytes on a
  character boundary, so a multi-byte character at the cut no longer
  crashes it (#4, thanks @rudycelekli).
- The graph engine's MCP server answers `initialize` with a protocol
  version it speaks: the client's when it knows it, otherwise 2025-06-18,
  instead of echoing whatever the client sent (#5, thanks @rudycelekli).
- The app's MCP endpoint over HTTP answers a method it does not serve with
  JSON-RPC's "Method not found" (-32601) instead of "Invalid params"
  (-32602); a refused call of a method it serves keeps -32602 (#6, thanks
  @rudycelekli).
- The engine's file ledger (`crates/graphicle-graph/migration/FILES.json`)
  marks each application module that has a passing comparison receipt against
  Graphify 0.9.91 as verified, with the Rust files that port it. The modules
  whose checks stop on the Linux host stay pending until the full run on
  macOS. The ledger's summaries beside it, `STATUS.md`, `AUDIT.json` and
  `PROGRESS.json`, now describe the 0.9.91 port instead of the 0.9.88 one.
- cargo-deny ignores RUSTSEC-2026-0367, RUSTSEC-2026-0368 and
  RUSTSEC-2026-0369 (zip 8.6.0, published after 0.1.1), with the reason: they
  affect only zip's extract-to-disk APIs, and Graphicle never extracts
  archives to disk; it reads archive members in memory. The ignores go once
  calamine, which still requires zip 8, moves to zip 9.

## 0.1.1 - 2026-10-15

Every crate is at 0.1.1, and `graphicle --version` prints
`graphicle 0.1.1 (core)`.

### Code graph

- The engine follows Graphify 0.9.91 (0.1.0 followed 0.9.88): the three
  releases 0.9.89 to 0.9.91 are ported. Against 0.9.91 the comparison checks
  have run on Linux, where the only checks that do not pass stop for reasons
  of that host; the full run on macOS is still to be made
  (`docs/benchmarks.md`). What changes in the graph:
  - Go: a package type whose methods span several files is one node, and
    the methods of a generic receiver (`func (b *Box[T])`) attach to `Box`.
    Member calls on a typed receiver are bound; when the receiver's type is
    declared in another repository (Go, PHP, TypeScript and JavaScript,
    Kotlin, Objective-C), the call is bound by `merge-graphs` and
    `global add`.
  - C#: `foreach` loop variables are typed; member calls bind only through a
    typed receiver; names that differ only by `_` or by case keep separate
    nodes; partial classes merge after colliding ids are separated; `new X()`
    no longer binds to a method named `X`.
  - Generic and template type parameters of Kotlin, Scala, C++ and Swift no
    longer create stub nodes. Objective-C `NS_ENUM` and `NS_OPTIONS` give an
    enum type with its members. An interface member links to every
    implementer.
  - JavaScript and TypeScript: a `.js` import resolves to a `.tsx` source
    when no `.js` file exists; a file-scope callback's parameters shadow
    same-named functions; a class field's type is scoped to its class.
    Python module calls (`mod.func()`) resolve inside methods and nested
    functions. A Rust `use crate::…::Name` edge lands on the declaration it
    imports. A symbol whose id spells another file's id keeps its own node.
  - Jinja templates (`.j2`, `.jinja`, `.jinja2`) are documents. An
    incremental build keeps an edge between two kept nodes even when its
    source file was not re-extracted, and sweeps orphaned placeholder nodes.
    Dedup keeps a document beside a same-named top-level file. A community
    keeps its curated or LLM label when its members mostly stay, and
    fallback labels go to `.graphicle_labels.pending.json`.
  - `hook install` also installs a `post-merge` hook; a read or search nudge
    names the graph nodes behind it; `pi guard install` adds a Pi guard
    extension; Ollama requests go to `/api/chat` (with a fallback for
    servers without it); colliding downloaded captures get numbered names.
  - The AST cache schema moves to 9, so the first build after upgrading
    re-extracts. The writer stamp in `graph.json` names 0.9.91.
- Go call binding reads each `go.mod` once per pass instead of once for
  every candidate of every Go call.

### How the engine holds its data

- During assembly, the per-file results, their raw calls and the edges are
  compact records, each value stored once in a table they share, instead of
  one JSON object each. The resolvers, the build, `graph.json`'s writer and
  the update path read them in place; the Java type passes borrow them
  instead of copying them.
- Community detection runs on the graph itself instead of on two copies of
  it; call binding and the member-call resolvers keep compact pair tables;
  the graph's indices and adjacency rows are compact; an update keeps its
  stored graph out of memory once it has read it; the per-file records are
  freed after the last pass that reads them, and freed pages go back to the
  system at stage boundaries.
- None of this changes the graph: on the repositories checked, `graph.json`
  is byte-identical with and without these changes. The graph changes under
  Code graph come from following Graphify 0.9.91.

### Agents and setup

Less fixed text in every agent request. In a pre-registered test of 0.1.0
(100 SWE-bench Verified tasks, Claude Code), the agents never called
Graphicle's tools, and setup's text was in every request. 0.1.1 makes no
token or accuracy claim; it trims that text.

- `graphicle setup` gives Claude Code five tools instead of 25:
  `query_graph`, `get_node`, `get_neighbors`, `recall_project` and
  `read_project_source` (`graphicle mcp --tools essentials`). Claude Code
  shows the names of a server's tools and its instructions in every request,
  and loads a tool only when the agent searches for it. The five come with
  one sentence of instructions on when to call them; their descriptions
  start with what an agent looks for and say how answers cite code.
  `setup --tools all` gives Claude Code every tool, as 0.1.0 did; the other
  clients keep every tool.
- The guard hooks that point Claude Code's searches to the graph tools are
  off unless asked for: `setup --guards` (or `connect --guards`). Running
  setup again without it removes the guard hooks an earlier setup installed.
- The Graphicle section of `AGENTS.md` and `CLAUDE.md` is two lines. Setup
  adds none when Claude Code is the only agent connected; a section an
  earlier setup wrote is kept and brought up to date. `setup --tools all`
  writes 0.1.0's section, whichever agents are connected.
- `graphicle graph benchmark` calls its figure what it is: the size of the
  whole corpus over the size of one graph answer, a size ratio, not what an
  agent saves. Its JSON keeps Graphify's field names.
- Claude Code's session-start hook prints nothing when the project has no
  reviewed memory, no open work and no stale memory.
- `graphicle connect claude-code` without `--tools` keeps an existing entry
  of the same store and project whatever its tool set, such as 0.1.0's
  setup's or `setup --tools all`'s, instead of refusing it. `doctor --fix`
  brings back an entry that names no `--tools`, or is gone, as the whole
  server.
- `setup --tools all --guards` restores 0.1.0's setup for Claude Code (every
  tool, 0.1.0's instructions and replies, the guard hooks, 0.1.0's
  `AGENTS.md` section) except for two things: the session-start hook still
  prints nothing when there is nothing reviewed, open or stale (0.1.0
  printed the last captured turns), and `graphicle context` in a terminal
  still puts test files last by default (`include_tests: true` through
  `graphicle call context` gives 0.1.0's order). A call that passes
  `structured` to recall or a graph tool, which 0.1.0 refused, is
  accepted and changes nothing.

### Context and recall

- `context`, and `recall_project` in the essentials set, present excerpts
  of test files after the other excerpts unless the question names a test
  file or a test identifier (`tests/...`, `test_x`, `x_test.py`,
  `conftest.py`, `TestX`). A test file is one in a `test`, `tests` or
  `testing` folder, or a `test_*`, `*_test.py`, `*_tests.py` or
  `conftest.py` file; other languages' test names (`x_test.go`,
  `x.spec.ts`) are not moved. Only the order changes: the budget goes to
  the excerpts in rank order first, so a packet holds the same excerpts as
  0.1.0's. The new `include_tests` argument of `context` (`graphicle call
  context --arguments`, the HTTP API) sets this per call: `"auto"` (the
  default), `false` (always after) or `true` (0.1.0's order), and the
  packet's `test_files` says what applied.
- With `--tools essentials`, `recall_project` answers with the packet's
  Markdown only, at most 12,000 characters (room is kept for the line that
  lists the `needs` the excerpts do not cover), and the graph tools with
  their text and citations, without a `structuredContent` copy.
  `structured: true` returns 0.1.0's reply; the Python SDK asks for it.
  Every other set (`--tools all`, which the other clients and `graphicle
  serve` get by default, `graph` and `memory`) answers exactly as 0.1.0
  did: the same reply, recall at the budget asked for and in 0.1.0's
  order.

### Security

- rustls 0.23.46, which fixes RUSTSEC-2026-0346, RUSTSEC-2026-0347,
  RUSTSEC-2026-0348 and RUSTSEC-2026-0349 (TLS, published on 14 October
  2026). Only the lock files change.

### Windows (preview)

- Release archives for Windows on x86-64,
  `graphicle-<version>-x86_64-pc-windows-msvc.zip`, with `graphicle.exe` and
  `graphicle-graph.exe`. The programs link the C runtime statically, so they
  run without the Visual C++ runtime; the release build checks that they
  import Windows system libraries only.
- `install.ps1` installs both programs from PowerShell (Windows PowerShell 5.1
  or PowerShell 7) into `%LOCALAPPDATA%\Programs\Graphicle\bin`, checks the
  archive's SHA-256 first, adds the folder to the user `PATH` and needs no
  administrator rights. Run again, it replaces programs that are running.
- Continuous integration builds and lints each crate on Windows and runs its
  tests.
- `docs/install.md` covers installing and building on Windows, what the
  preview leaves out, and troubleshooting. Without Python 3.14 or the
  `zstd` program, `prepare_parser_sources.py` on Windows suggests
  `py -m pip install zstandard`.
- The engine and the `graphicle` program on Windows: one path spelling with
  `/` in the paths the engine reports, path comparisons that ignore case and
  `\\?\` prefixes, network-share (UNC) roots, Graphify's Windows lock for
  rebuilds started by Git hooks, Ctrl+C and Ctrl+Break handling, and colour
  only where the console supports it.
- On Windows, `graphicle` keeps its data in `%LOCALAPPDATA%\Graphicle`, finds
  the settings of Claude Desktop (including its Microsoft Store build) and
  VS Code in their Windows folders, finds programs with the extensions in
  `PATHEXT`, and starts background work without a console window.
- On Windows, the memory library reads a source named in a graph only when
  its path stays inside the project folder: no drive, network-share, rooted
  or `..` paths.
- The Windows programs are not code-signed yet. `docs/code-signing.md` is the
  code signing policy for the planned signing through the SignPath
  Foundation; the release workflow's signing job stays off until then.

### Other changes

- On macOS, `graphicle-graph` and `graphicle` use the system memory
  allocator, because mimalloc 3.3.2 can crash on Apple silicon when memory
  is freed after worker threads exit (microsoft/mimalloc#1306). Linux and
  Windows keep mimalloc.
- A new-release notice, like the GitHub CLI's. Once a day, when you run a
  command in a terminal, `graphicle` asks GitHub's releases API for the
  latest release. That is one GET request, with no code or data in it; when
  GitHub has not answered by the time the command ends, the request is made
  again with the next command. When the release is newer, `graphicle` prints the version, the upgrade
  command for your install and the release page on standard error. It never
  checks from the MCP server, `serve`, hooks, session capture, `update`,
  CI, or JSON or piped output. `GRAPHICLE_NO_UPDATE_NOTIFIER=1` or
  `graphicle config set update_notifier false` turns it off.
- The repository's `.gitattributes` checks text files out with LF line
  endings on every system.

## 0.1.0 - 2026-10-14

The first public release. Every crate is at 0.1.0, and
`graphicle --version` prints `graphicle 0.1.0 (core)`.

### Code graph

- A Rust graph engine compatible with Graphify 0.9.88. Comparison checks run
  Graphify's own tests against the engine and compare its results with those
  of Graphify 0.9.88; `docs/benchmarks.md` gives the checks, their results
  and their limits. Where an upstream test fails against the engine,
  `docs/evidence/upstream-tests-known-differences.md` records it with the
  reason, and `docs/evidence/upstream-tests-not-run.md` lists the test files
  the checks do not run.
- `graphicle graph <command>` runs the engine (`graphicle graph update .`),
  and the engine also ships as its own program, `graphicle-graph`.
- Opt-in additions beyond Graphify: `--precise`, `extract --docs`,
  `extract --routes`, `retrieve` and `serve --watch`.

### Memory, recall and answers

- `graphicle sync` keeps a project's code, documents and captured agent
  sessions in one local store that keeps the original sources, and builds the
  project's code graph.
- `graphicle context` recalls cited excerpts for a question from the sources,
  the code graph and reviewed memory, within a token budget.
- `graphicle ask` answers with numbered citations, using a model you
  configure (`graphicle config set model anthropic`, `openai` or `ollama`;
  for Ollama it uses the server `OLLAMA_HOST` names, as `ollama` does, and
  says whether Ollama answers and has the model). Without a model it prints
  the cited sources and how to add one.
- Ollama requests turn thinking off (`think: false`), so a thinking model
  such as the default `qwen3:4b` writes its answer within its output limit.
- A citation whose quote differs from its source only in spaces, tabs or
  line breaks (a model that joins blank lines, re-indents code or writes a
  line break as `\n`) is kept as the source's exact text, in `ask`,
  `--workflow reflect` and its review; an answer that abstains drops the
  citations it cannot support. Quotes that differ in anything else still
  fail the answer.
- When a provider refuses a request, the error gives the provider's own
  reason (never the key), and `ask` says what to change. `--workflow reflect`
  says when its review call failed and the answer was not reviewed, and
  shows the file and lines of each citation.
- With `structured_output`, the Anthropic transport offers its result tool
  without forcing it, and the OpenAI-style protocols leave `minLength` and
  `maxLength` out of the schema they send; every reply is still checked
  against the whole schema.
- Agents can propose memories, which wait as drafts until a person reviews
  them.
- A memory whose quote lies inside one function, class or method is anchored
  to that code symbol, by its `stable_id` and revision. Reviewed memories,
  retained answers and member notes then go stale when the symbol changes or
  is removed (`symbol_changed`, `symbol_missing`): an edit to the symbol's
  lines, from its start up to the next definition (or the end of the file),
  withholds the memory, and other edits to the file do not. A file moved
  with its content unchanged is followed, and while a sync waits for its
  code graph a memory on a changed file is held back (`projection_pending`).
  `propose_memory` and `note` also take `symbols` (a `stable_id`, or a path
  and qualified name) and whole-file `files`, and the app's memory form has a
  symbol picker.
- Recall across the projects of a workspace: `graphicle context
  --workspace`, `scope: "workspace"` over HTTP and the SDK, and a workspace
  MCP connection search every project the access key may read (at most 8 by
  default, `max_projects` up to 32). Each project is read and authorized as a
  search of that project alone would be, the results are interleaved within
  one token budget, every excerpt names its project, and the imports between
  the projects that `sync` recorded are followed one step each way. Answers
  (`ask`) still read one project.
- Documents are linked to the code they name: a Markdown code span, a
  reStructuredText role or literal, and the inline code of Jira, Notion,
  Confluence, GitHub and Slack text link to a code symbol when they name
  exactly one definition, in the document's own project first, then in the
  other projects of its workspace; plain words and ambiguous names never
  link. `graphicle call graph.doc_links` lists the links, counting only what
  the caller may read. Recall counts the links of the excerpts it returns
  without following them; `doc_links: true` (or `graphicle mcp --set
  doc_links=true`) follows them one step each way, and `doc_links: false`
  reads none and keeps documents out of the code-graph part of the ranking.

### Setup and agents

- `graphicle setup` sets up a repository in one command: it syncs the folder,
  builds the code graph, connects the coding agents it finds (Claude Code,
  Codex, Cursor, VS Code and Claude Desktop), turns on session capture, adds
  a marked section to `AGENTS.md` and marked lines to the Git hooks. It lists
  and backs up every file it changes (the backups, and a
  `.claude/settings.local.json` it creates, stay out of Git), and is safe to
  run again.
- `graphicle connect`, `graphicle disconnect`, `graphicle uninstall` and
  `graphicle doctor` (with `--fix`) connect agents, undo what Graphicle added,
  and check and repair the setup. `graphicle uninstall` says how to remove the
  program the way it was installed (Homebrew, Docker, Cargo, the install
  script or a source build).
- In an agent's settings file with comments (VS Code's JSONC), Graphicle's
  entry goes on lines of its own, every comment stays where it was, and
  `graphicle disconnect` keeps comments you changed while the agent was
  connected.
- Each agent that `graphicle setup` (or `graphicle connect` without an
  agent) connects gets one MCP server with the code-graph, recall and memory
  tools, `graphicle mcp --allow-learning`: the agent may propose memory
  drafts, which a person approves before recall uses them. Claude Code also
  gets two guard hooks that point its searches and file reads in a project
  with code to the graph tools. `connect <agent> --graph` sets up one agent
  the same way, and `disconnect` removes the hooks too.
- The guard hooks nudge each agent at most five times a session, the first
  time always; `GRAPHICLE_HOOK_NUDGE_CAP` sets another limit, and `0` lifts it.

### MCP

- `graphicle mcp` is one MCP server for any MCP client: `recall_project`
  returns original source evidence and current reviewed memory,
  `--allow-learning` adds the tools that propose memory, and the graph
  engine's tools answer under Graphify's names (`query_graph`, `get_node`,
  `get_neighbors`, `get_community`, `god_nodes`, `graph_stats` and
  `shortest_path`), with its resources, from the code graph that
  `graphicle sync` records. Each graph answer is the engine's text, then a
  citations block that names up to 20 of its nodes by file, line range and
  `stable_id` (the one `recall_project` cites for that code);
  `structuredContent` cites up to 100, with the checked byte range of each
  node whose file has not changed since. `as_of` reads an earlier graph.
  `--tools graph|memory` serves one half of the tools, `--set` fixes a
  `recall_project` switch for the session, and `--pull-requests` adds the
  engine's pull-request tools, which run the GitHub CLI, over stdio in the
  project's folder. While `graphicle serve` runs, the same tools answer over
  HTTP (`serve --mcp-tools` and `--mcp-set` work as `--tools` and `--set`).
- A connection to one project keeps recall, source reads and memory on that
  project. Its graph tools and graph-history tools read another project's
  code graph when `repo` names one the access key can read (over stdio, also
  a synced folder), and `graphicle mcp` runs with the key saved in the data
  folder, normally the owner's, which reads every project. To keep an agent
  to one project, connect it over HTTP with its own key and revoke that
  key's access to the other projects with `graph.access` (`docs/mcp.md`). A
  workspace connection (`graphicle mcp --workspace`, or
  `POST /mcp/WORKSPACE_ID`) serves every project the key can read, with
  `list_projects`.

### Freshness

- Before `ask`, `context`, `status` and each agent tool call (through
  `graphicle mcp` or the HTTP MCP endpoint of `graphicle serve`), Graphicle
  checks whether files changed since the last sync. It syncs a few changed
  files first, and otherwise says how many changed and suggests
  `graphicle sync`. The Git hooks sync in the background after commits,
  merges and checkouts. `graphicle config set freshness off` turns the check
  off.
- A sync of a folder whose files did not change converts no document again,
  so the check stays quick on large folders.

### Web app and SDKs

- `graphicle open` opens the web app (ask, sources, memory and the code
  graph); `graphicle serve` runs it with the HTTP API, also as a shared
  server for a team that manages its own keys (`docs/team.md`).
- A Python SDK (`graphicle-sdk`, imported as `graphicle_sdk`) and a
  TypeScript SDK talk to a running server. Neither is on a package registry
  yet; install them from `sdk/`.

### Limits

- A project takes up to 100,000 text files and 1 GiB of text, and a text
  file up to 8 MiB. Whoever runs Graphicle can raise each limit with
  `limits.<name>` (`graphicle config set limits.project_files 250000`) or a
  `GRAPHICLE_MAX_<NAME>` environment variable; a request through the API or
  MCP cannot. A project over a limit is refused with a message that names
  the limit and its setting, and one file never stops a sync: files past the
  analysis limits are analysed as far as the limits allow, and files that
  cannot be read are listed with the reason. `graphicle config show` lists
  the limits in effect, and `graphicle config get limits.<name>` prints one.
  The "Limits" section of `docs/configuration.md` lists every limit.

### Privacy

- Local first: the store is one SQLite file in your data folder, and the app
  and its HTTP and MCP endpoints listen on 127.0.0.1 by default.
- No telemetry and no account. Graphicle connects to the network only when
  you ask it to, for example for a model you configure or a connector you
  add; the README's privacy section lists every case.
- Sync respects `.gitignore` and skips symlinks, `.env` files, keys and
  credential files; session capture redacts credentials before anything is
  stored.

### Install

- macOS and Linux (glibc), on arm64 and x86_64: the Homebrew tap
  (`brew install graphicle-ai/tap/graphicle`), the install script, which
  checks each archive's SHA-256, or the release archives. There is no
  native Windows build yet.
