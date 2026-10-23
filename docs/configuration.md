# Configuration

Graphicle works without any configuration. You configure it only to choose
where the data lives, to add a model for answers and extraction, or to change
how retrieval ranks and packs evidence.

## The data folder

| System | Default |
|---|---|
| macOS | `~/Library/Application Support/Graphicle` |
| Linux | `$XDG_DATA_HOME/graphicle`, or `~/.local/share/graphicle` |

`--data-dir DIR` on any command, or the environment variable
`GRAPHICLE_DATA_DIR`, selects another folder.

| File | What it holds |
|---|---|
| `reviews.sqlite3` | The store: sources and their revisions, memory, jobs and the recorded graph history |
| `local-session.json` | The local workspace's ID, its owner access key and the current project. It is readable by you only; keep it private |
| `vault.key` | The key that encrypts provider credentials saved in the app |
| `models.json` | Optional model profiles (see below) |
| `agent-changes.json` | What `setup` and `connect` changed outside the data folder, for `graphicle disconnect` (with `agent-changes.lock`, which keeps two commands from changing it at once) |
| `server.json` | While `graphicle serve` (or `open`) runs: its port and a secret, so that `graphicle open` signs in only to this folder's server. It is readable by you only |
| `version.json` | The newest Graphicle version that used the folder |
| `gf-cache/` | Graph caches, and each project's saved code graph for questions (`query/`), which are rebuilt when needed |
| `backups/` | The backups `graphicle backup` writes when you give it no path; `graphicle offboard` removes a project from them |

`graphicle sync` creates the local workspace the first time it runs, so you do
not need `graphicle init`.

To back up while the app is running, run `graphicle backup /safe/place/graphicle.sqlite3`,
or `graphicle backup` to write it to the data folder's `backups/` folder (a
backup there goes with the data folder, so keep a copy elsewhere too).
`graphicle offboard --project PROJECT` removes one project from the store and
from the backups in `backups/`, with a hash-chained receipt; without
`--apply PLAN_HASH` it only shows its plan.
`graphicle --data-dir NEW_FOLDER restore /safe/place/graphicle.sqlite3`
restores into a new folder and never overwrites existing data. A backup leaves
out provider secrets and the vault key, so reconnect providers after a restore.

## Models

These features need a model:

- `graphicle ask`;
- the app's **Ask** box;
- the `extract` and `refresh` jobs, which propose memory from sources;
- the `observer_notes` job;
- the dense retrieval arm of answers.

Syncing, `graphicle context`, both MCP servers and the code-only graph work
without one.

You can add a model in any of three ways:

- **With one command:** `graphicle config set model anthropic` uses the key
  in `ANTHROPIC_API_KEY`, `graphicle config set model openai` the key in
  `OPENAI_API_KEY`, and `graphicle config set model ollama` a local Ollama:
  the server `OLLAMA_HOST` names, as `ollama` itself uses it, else
  `http://127.0.0.1:11434` (it says whether Ollama answers and has the model,
  and otherwise to run `ollama serve` or `ollama pull MODEL`). `--model NAME`
  (or `ollama:NAME`) chooses the model and `--url URL` the address. The
  configuration records the name of the environment variable, never the key.
  `graphicle config show` prints the settings, and `graphicle config path`
  the file that holds them. `graphicle setup` offers this step when it finds
  one of these keys or a local Ollama.
- **In the app:** open **Connections → Answering models → Configure model**.
  The app saves the profile in the workspace, and it saves the API key in the
  encrypted vault.
- **In a file:** write `models.json` in the data folder, which Graphicle reads
  automatically, or pass `--config PATH`. `graphicle config example` prints a
  starting point:

```json
{
  "default_model": "local",
  "embedding_model": "embedding",
  "worker_concurrency": 1,
  "models": {
    "local": {
      "protocol": "ollama",
      "base_url": "http://127.0.0.1:11434",
      "model": "qwen3:4b",
      "input_limit": 16000,
      "output_limit": 2048
    },
    "embedding": {
      "protocol": "ollama",
      "base_url": "http://127.0.0.1:11434",
      "model": "nomic-embed-text",
      "input_limit": 16000,
      "output_limit": 2048
    },
    "api": {
      "protocol": "responses",
      "base_url": "https://api.openai.com/v1",
      "model": "YOUR_MODEL",
      "key_env": "OPENAI_API_KEY",
      "input_limit": 16000,
      "output_limit": 2048
    }
  }
}
```

| Field | Meaning |
|---|---|
| `default_model` | The profile used for answers and extraction |
| `embedding_model` | The profile that embeds passages for the dense arm of answers. `graphicle run embed --wait` builds the vectors. Each run makes at most `--max-calls` calls (default 4, at most 32), so a large project needs several runs |
| `worker_concurrency` | Jobs run at once, from 1 to 64 (default 1) |
| `models.NAME.protocol` | `ollama`; `chat` (Chat Completions, for OpenAI-compatible servers such as vLLM or llama.cpp); `responses`; `anthropic`; `gemini`; `azure_chat`; `azure_responses`. `local_onnx` needs a build with `local-inference` |
| `base_url` | HTTPS, or HTTP on loopback. A plain-HTTP host on your network must be listed in `GRAPHICLE_PROVIDER_HTTP_HOSTS` (exact host names or addresses, separated by commas). Such a profile cannot have a `key_env` |
| `model` | The provider's model name |
| `key_env` | The name of the environment variable that holds the API key. The key itself never goes in the file |
| `input_limit`, `output_limit` | Token limits: 512 to 128,000 (default 16,000) and 64 to 16,000 (default 2,048) |
| `temperature` | Optional, from 0 to 2 |
| `structured_output` | `true` by default. Set it to `false` for servers without schema-constrained output; replies are still validated. With `anthropic`, `true` offers the result as a tool the model is asked to call (Claude Opus 5.5, Claude Sonnet 5.5 and Claude Fable 5.1 refuse a forced tool call); `graphicle config set model anthropic` writes `false`, which asks for the JSON in the prompt |
| `extra_body` | `chat` and `azure_chat` only: up to 16 extra request fields, such as `chat_template_kwargs` |

An OpenAI-compatible server, such as vLLM, llama.cpp or LM Studio, takes a
`chat` profile. Two commands write one; the second makes it the answer model:

```sh
graphicle config set models.local '{"protocol":"chat","base_url":"http://127.0.0.1:8000/v1","model":"NAME"}'
graphicle config set default_model local
```

Add `"key_env":"VARIABLE"` when the server needs a key. A server that speaks
OpenAI's Responses API, such as vLLM, also works with `graphicle config set
model openai --url URL --model NAME`, which sends the key in `OPENAI_API_KEY`.

Graphicle sends `ollama` requests with thinking turned off (`think: false`),
so a thinking model such as `qwen3:4b` writes its answer within
`output_limit` instead of spending it on reasoning. When a provider refuses a
request, `ask` prints the provider's own reason, without the key.

Graphicle refuses unknown fields. The file can also set `max_cpu_jobs`,
`max_model_jobs` and `max_queued_bytes_per_repository` to bound background
work.

Every model call is bounded. `graphicle ask --max-calls N` (default 4) caps a
question's calls, and each job records the calls and tokens it used.
`graphicle jobs` lists them.

## Asking questions

`graphicle ask "question"` makes one model call. The call reads the retrieved
excerpts and answers with exact quotes (the default `basic` workflow). To let
the model search and read further before it answers, and check its answer, run
`graphicle ask "question" --workflow reflect`. With `--retain`, that answer is
saved as a memory draft for review.

## Retrieval

`graphicle context` and the memory server's `recall_project` rank the
project's sources with BM25 and add current reviewed memory. Answers rank the
original sources the same way. Three options are on by default in both:

- `stem: true`: BM25 over Porter stems.
- `speaker: "auto"`: a ranking over one speaker's turns in conversations. For
  example, the assistant's turns rank for "what did you recommend", and the
  user's turns rank for most other questions.
- `pack_focus: true`: each excerpt centres on the paragraphs that match the
  question.

Excerpts of test files come after the other excerpts unless the question
names a test file or a test identifier (`tests/...`, `test_x`, `x_test.py`,
`conftest.py`, `TestX`): `include_tests` is `"auto"` by default, `false`
always puts them after and `true` keeps 0.1.0's order. A test file is one in
a `test`, `tests` or `testing` folder, or a `test_*`, `*_test.py`,
`*_tests.py` or `conftest.py` file; other languages' test names
(`x_test.go`, `x.spec.ts`, `__tests__/`) are not moved. Only the order
changes: the budget goes to the excerpts in rank order first, so the packet
holds the same excerpts as with `true`. The packet's `test_files` says what
applied. Over MCP, the essentials set (what setup gives Claude Code) uses
this default; `--tools all`, `graph` and `memory` answer `recall_project` in
0.1.0's order.

A code question ranks the project's source files first (`code_first`,
`"auto"` by default). A question is a code question when it names code (a
backtick span, a `snake_case` or `camelCase` name with two lowercase letters
before its capital, `Type::member`, `call()` or a code file) or reads like an issue or bug report (a code block, a stack
trace or error line, or two words such as crash, error, fails, expected or
regression). Its documentation files (Markdown, reStructuredText, AsciiDoc,
plain text, `README`, `CHANGELOG`, `LICENSE`, files under `doc/`), test
files and vendored or generated code (`vendor/`, `third_party/`,
`node_modules/`, `x.min.js`, `x.pb.go`, lock files) count a quarter of
their fused score in the ranking, before the excerpts are chosen and the
budget is spent, and the code-graph arm leaves Markdown nodes out. A test
file is one under a `test`, `tests`, `testing`, `__tests__`, `spec`,
`testdata` or `fixtures` folder, a `Foo.Tests` or `FooTests` project, or one
named like a test in Rust (`tests.rs`, `x_test.rs`), Python (`test_x.py`,
`x_test.py`, `conftest.py`), Go (`x_test.go`), Java, Kotlin and Scala
(`FooTest.java`, `FooIT.java`, `TestFoo.java`), C# (`FooTests.cs`),
JavaScript and TypeScript (`x.test.ts`, `x.spec.js`), C and C++
(`test_x.c`, `x_test.cc`, `x_unittest.cpp`) or Ruby (`x_spec.rb`). These
keep the order they had:

- a question about documents, notes or decisions (it names a
  documentation file or uses words such as documentation, docs, readme,
  notes or decided) that does not read like a report;
- test files, when a question that does not read like a report names a
  test, or the call sets `include_tests: true`;
- a test or document that defines a code name the question names (one
  with an underscore or an inner capital, as the identifier arm finds it in
  the code graph) when no source file defines that name, such as a test
  helper or a fixture.

`code_first: false` keeps that order for every question, and `true` ranks
source files first for every question. The packet's `code_first` says what
applied, why, and how many files of each kind it lowered.

A code question is searched by its content, not its instructions
(`query_focus`, `"auto"` by default; `true` for every question, `false`
for none). When it marks its content, the query is the marked text, every
word kept, followed by the names of code in its prose outside the marks
(backtick spans, identifiers and file paths, leaving out fenced examples and
placeholders such as `ClassName.method_name` or `path/to/file.py`). A mark
is a tag of its own (`<issue>` ... `</issue>`, `<pr_description>` ...),
opening at the start of a line, with text after it or not, and closing
anywhere after it (HTML elements such as `<details>` do not count), or a pair
of delimiter lines (`--- BEGIN ISSUE ---` ... `--- END ISSUE ---`, drawn with
`-`, `=`, `#`, `*` or `~`). The marks are ignored when the text outside them
is longer than 2,000 characters and they hold less than 40% of the question.
A question without marks leaves out the general words of instructions, such
as find, locate, fix, resolve, change, edit, create, delete, implement,
list, explain, task, issue, problem, question, file, function, method, class
and please. Either way web addresses are left out, except a code host's link
to a file (`https://github.com/o/r/blob/<ref>/src/x.rs#L10`, GitLab's
`/-/blob/`), which becomes the file's path, and so are commit hashes.
Identifiers, calls, quoted words and paths always stay. BM25, the code-graph
arm, the files read past the passage bound, a local reranker and the
excerpts' focus read the query; the packet's `question` is the question, and
its `query_focus` says what was marked and left out.

The identifiers and file paths a code question names rank their files
(`identifier_arm`, `"auto"` by default; `true` for any question, `false`
for none). Its names are every identifier of three or more characters in
a code block or span that is not a common English word or keyword, and in
prose the `snake_case`, `SCREAMING_CASE`, camelCase and PascalCase words
with a capital inside, calls (`name(`) and qualified names (`a.b`, `a::b`,
`a->b`, `a#b`), whole and by their parts written as code; its paths are the
file names and paths with a known extension. Each name is matched as a
whole word, case-sensitively, in the project's passages, and found at its
definitions in the code graph (unless `graph_retrieval: false`). A file
scores, for each of the 40 most specific names it holds,
`ln(1 + files / files holding the name)`, twice that where it defines the
name, plus twice the weight of a name only it holds when the question names
its path. Files come in score order, each with its two best passages, as
the arm `identifiers` of the fusion (weight 1; `arm_weights` sets another).
The packet's `identifier_arm` lists the names and paths used and how many
files ranked. With the identifier route on, a question that asks more than
where its names are keeps the retrieval as before, and its `route` now
says `names_fused` when the arm ranked them.

`graphicle context` prints at most 8,400 characters by default, so a prompt
hook that adds the packet under a header stays below the 10,000 characters
Claude Code shows in full. `--max-chars` sets another size; with only
`--max-tokens`, the packet holds 4 characters per token, as before.

After its excerpts, a packet lists the paths of the files that rank next and
that no excerpt quotes, one per line under "Next ranked files"
(`more_files`: 0 to 40; left out, 10 for a code question and 0 for any
other). Their room is kept from the excerpts' share of the budget, at most
an eighth of it, so the packet still fits `max_chars` and `max_tokens`. The
packet's `more_files` lists the paths.

`code_first`, `query_focus`, `identifier_arm` (each `"auto"`, `true` or
`false`) and `more_files` (0 to 40) are also flags of `graphicle context`
(`--code-first false`, `--query-focus true`, `--identifier-arm auto`,
`--more-files 0`) and switches of `recall_project` over MCP, beside its
published arguments; `graphicle mcp --set code_first=false` (or
`more_files=0`, `query_focus=auto`) fixes one for a server, and every packet
echoes them in `mechanisms.switches`.

A question over 16,000 bytes is shortened, not refused: to the text it
marks with its own tags when that fits, else to its beginning, cut at a line
end, followed by the identifiers and file paths of the text cut (at most
3,000 bytes of them). The packet's `question_shortened` says so:
`{"bytes": ..., "tagged_content": ..., "kept_bytes": ..., "names_appended":
...}`.

`notes_first: true` turns on three changes for a call, which are off by
default (`graphicle context --notes-first`; over MCP, `recall_project` takes
it beside its published arguments, and `graphicle mcp --set
notes_first=true` turns it on for a server):

- A question that names no code (no backtick span, `snake_case` or
  `camelCase` name, `Type::member`, `call()` or code file) takes the first 50
  files of the fused ranking, one passage each. Code files keep their
  places; the notes (Markdown, text, PDF and other documents) among them are
  ordered again among the notes' places: a note whose name or folder matches
  the question's words, whose headings match, or that has a newer date (the
  latest ISO date, up to today, in its path or text) comes before the other
  notes. A question that names code keeps the code ranking.
- A packet quotes one window of a file, and one of files with identical
  bytes; the places freed go to the next files. `include_tests: true`, which
  `--tools all`, `graph` and `memory` send for `recall_project`, keeps 0.1.0's
  windows as ranked whatever `notes_first` says, and so does an explicit
  `origin_quota` (`false`: at most two windows per file).
- Each excerpt of a project file says when its file last changed, in its
  citation: `Source [repository_file]: notes/node.md:75 (changed 2026-10-15
  05:10 UTC)`. The date is that of the last commit that changed the file
  when Git tracks it and the working tree has not changed it, otherwise the
  file's modification time, read in the folder the project was last synced
  from. A note may follow the citation: `Note: may be stale: <file> (defines
  <name>) changed <date>`, when the excerpt names code (a document link, as
  `doc_links` records it, written as code: with `_`, `::`, `.`, `/` or `(`,
  or a capital inside it) whose file changed after the excerpt's file last
  did. The date is the file's; the change may not touch the named
  definition. Excerpts are not compared with one another, and the dates do
  not change their order. `source_age: true` or `false` turns the dates on
  or off by themselves; `--tools all`, `graph` and `memory` answer
  `recall_project` without them.

The packet's `notes_first` says what applied (`off` when the call did not
turn it on), and `source_age` what was dated and marked; each dated source
carries `changed`, `changed_at` and `changed_basis` (`git` or `mtime`).

`identifier_route` answers a question that is an identifier, or names
identifiers and only asks where they are defined or used, from the code
graph instead of the full retrieval: each name's definitions and the places
that refer to it, as `path:line` lines within the call's budget, and one
line with the titles of the reviewed memories linked to those definitions
(none when the call sets `memory_retrieval: false`). The packet has no
excerpts: its `evidence` and `sources` are empty. `graphicle context` and,
over MCP, `recall_project` in the essentials set (what setup gives Claude
Code) take the route unless told otherwise; the HTTP API, `graphicle call
context` and the SDKs take it only when a call sets `identifier_route:
true`, and so do `--tools all`, `graph` and `memory`.

A name is a backtick span that reads as an identifier, a `snake_case` or
`SCREAMING_CASE` word, a camelCase or PascalCase word with a capital inside
it (`parseConfig`, `ParseConfig`), a dotted or `::` path (`os.path.join`,
`Store::open`), or a call `name()`. A name alone takes the route. Other
words may only ask where the names are or what uses them, and one of them
must: `where is Store::open defined`, `who calls parse_config()`,
`references to ParseConfig`, `find the callers of run()`. These keep the
full retrieval:

- a question in plain words, a single plain word (`parser`) included;
- a question that asks for more than where a name is or what uses it, such
  as its content, its members, what it calls or where it is mentioned (`how
  does parse_config() handle a missing file?`, `what is MAX_NAMES?`, `list
  the fields of RecallArgs`, `callees of parse_config`), whose excerpts rank
  the named code already;
- a call with `needs` or `graph_retrieval: false`, a project without a code
  graph, and a question none of whose names the code graph defines in the
  files the call may read (`include`).

`graphicle context --no-identifier-route` keeps the retrieval for one call;
over MCP, `recall_project` takes `identifier_route` beside its published
arguments, and `graphicle mcp --set identifier_route=false` (or `true`)
fixes it for a server. The packet's `route` says which path ran:
`{"path": "identifier", "names": [...], "not_found": [...]}` with each
name's definitions and references under `symbols` and the linked memories
under `linked_memories`, or `{"path": "retrieval", "reason": ...}`, the
reason being `plain_words`, `asks_more`, `not_in_code_graph`,
`no_code_graph`, `needs`, `graph_retrieval_off`, `too_many_names`, `off` or,
for a workspace recall, which keeps the retrieval, `workspace`.

Each packet lists the defaults it applied in `retrieval_defaults`. To change an
option for one call, send the request through `graphicle call`. Setting an
option to `false` turns it off:

```sh
cat > question.json <<'EOF'
{"question": "How do we deploy?", "max_tokens": 6000, "stem": false, "pack_order": "chronological"}
EOF
graphicle call context --arguments question.json
```

Other options include the following:

- `pack_order: "chronological"` presents the excerpts in date order.
- `compact_citations: true` leaves hashes and IDs out of the rendered text.
- `origin_quota` caps the excerpts taken from each origin.
- `query_variants` adds rephrased queries.
- `reading_guidance: true` adds guidance on weighing what users said against
  what assistants suggested.
- `doc_links: true` follows the links between documents and the code they
  name (a Markdown code span, or a connector's inline code, that names one
  definition): an excerpt of a document brings in the definitions it names,
  and an excerpt of code brings in the passages that name its definitions.
  It is off by default. The links are still recorded: each packet counts
  those of its excerpts in `doc_links`, and `graphicle call graph.doc_links`
  lists them. `graphicle mcp --set doc_links=true` turns it on for every
  `recall_project` call of that server ([MCP](mcp.md)). `doc_links: false`
  also keeps documents out of the code-graph part of the ranking.
- `symbol_links: true` follows the links between reviewed memory and the code
  symbols it is anchored to, one step: an excerpt of code brings in the
  current reviewed memories linked to a symbol inside it, and a reviewed
  memory brings in the current code of the symbols it is linked to. It is off
  by default: each packet counts the links it did not follow in
  `symbol_links`. `graphicle mcp --set symbol_links=true` turns it on for
  every `recall_project` call of that server. `symbol_links: false` reads no
  such link.

These need a build with the `local-inference` feature:

- **Dense retrieval in `context`.** A local embedding profile
  (`--local-embedding-profile NAME`) adds a dense arm at weight 0.5.
- **Reranking.** A local cross-encoder (`--local-reranker-profile NAME`)
  reranks the results. It is off by default and not recommended for a project
  that holds one conversation's history. It failed its held-out check there
  and took about 20 s per question on a server CPU.

[crates/graphicle/LOCAL_INFERENCE.md](../crates/graphicle/LOCAL_INFERENCE.md)
describes both.

Answers use the `embedding_model` profile, when one is set, for their dense
arm.

## Limits

Graphicle Core runs on your machine, so its limits are generous and you can
change them. Whoever runs Graphicle sets them: the `limits` object of
`models.json` in the data folder (or of the `--config` file), or an
environment variable, which wins over the file. On a shared server the
operator's file and environment decide; a request through the API or MCP
cannot raise them.

| Limit | Default | Largest value | Setting | Environment variable | Past the limit |
|---|---|---|---|---|---|
| Text files per project | 100,000 | 10,000,000 | `limits.project_files` | `GRAPHICLE_MAX_PROJECT_FILES` | The sync is refused with a message that names the count, the limit, this setting and how to leave paths out |
| Text per project | 1 GiB | 1 TiB | `limits.project_bytes` | `GRAPHICLE_MAX_PROJECT_BYTES` | The same |
| Size of one text file | 8 MiB | 512 MiB | `limits.file_bytes` | `GRAPHICLE_MAX_FILE_BYTES` | The file is left out and listed under `skipped` |
| Structure analysis of one file | 4 MiB | 512 MiB | `limits.analysis_bytes` | `GRAPHICLE_MAX_ANALYSIS_BYTES` | Text only: no symbols, imports or calls. The text stays searchable. Listed under `limited` (`size`) |
| Calls collected from one file | 50,000 | 10,000,000 | `limits.file_calls` | `GRAPHICLE_MAX_FILE_CALLS` | The call list is cut at the limit. Listed under `limited` (`calls`) |
| Syntax nodes read in one file | 2,000,000 | 1,000,000,000 | `limits.file_nodes` | `GRAPHICLE_MAX_FILE_NODES` | The analysis stops at the limit. Listed under `limited` (`nodes`) |
| Declarations and links kept per file, and imports of Python, JavaScript, TypeScript, Rust and Markdown files | 16,000 each | 10,000,000 | `limits.file_symbols` | `GRAPHICLE_MAX_FILE_SYMBOLS` | The list is cut at the limit. Listed under `limited` (`symbols`) |
| One document file (PDF, Office, EPUB, HTML, image) | 128 MiB | 4 GiB | `limits.document_bytes` | `GRAPHICLE_MAX_DOCUMENT_BYTES` | A folder sync lists it under `skipped`; an upload is refused |
| Text of one document: extracted from a file, or one connector or uploaded document | 8 MiB | 512 MiB | `limits.document_text_bytes` | `GRAPHICLE_MAX_DOCUMENT_TEXT_BYTES` | A folder sync lists it under `skipped`; a connector sync lists it under `skipped` and goes on; an import or upload is refused with the document's name |
| An Office or EPUB archive, expanded | 512 MiB | 16 GiB | `limits.archive_bytes` | `GRAPHICLE_MAX_ARCHIVE_BYTES` | The document is not converted |
| Entries of an Office or EPUB archive | 40,000 | 10,000,000 | `limits.archive_entries` | `GRAPHICLE_MAX_ARCHIVE_ENTRIES` | The same |
| The documents of one folder sync together | 2 GiB | 1 TiB | `limits.folder_document_bytes` | `GRAPHICLE_MAX_FOLDER_DOCUMENT_BYTES` | The sync is refused with a message that names the limit and this setting |
| Document originals a project keeps, earlier versions included | 2 GiB | 1 TiB | `limits.retained_original_bytes` | `GRAPHICLE_MAX_RETAINED_ORIGINAL_BYTES` | Refused; `storage.prune` removes earlier versions |
| One agent session transcript | 512 MiB | 16 GiB | `limits.transcript_bytes` | `GRAPHICLE_MAX_TRANSCRIPT_BYTES` | The import is refused; capture passes over the file |
| One connector's snapshot | 512 MiB | 64 GiB | `limits.connector_snapshot_bytes` | `GRAPHICLE_MAX_CONNECTOR_SNAPSHOT_BYTES` | The connector sync is refused |
| Documents one connector lists, folders one Drive source walks, comments of one Jira issue | 40,000 | 10,000,000 | `limits.connector_documents` | `GRAPHICLE_MAX_CONNECTOR_DOCUMENTS` | The listing is refused; a Jira issue with more comments is listed under `skipped` |
| Messages of one Slack thread, blocks of one Notion page | 16,000 | 10,000,000 | `limits.connector_items` | `GRAPHICLE_MAX_CONNECTOR_ITEMS` | The thread or page is listed under `skipped` and the connector sync goes on |
| Structure nodes of one Jira document | 400,000 | 100,000,000 | `limits.connector_nodes` | `GRAPHICLE_MAX_CONNECTOR_NODES` | The issue is listed under `skipped` and the connector sync goes on |

```json
{
  "limits": {
    "project_files": 250000,
    "project_bytes": "4GiB",
    "file_calls": 100000,
    "document_text_bytes": "32MiB"
  }
}
```

`graphicle config set limits.project_files 250000` sets one limit in that
file, and checks it first. `graphicle config get limits.project_files` prints
the limit in effect, set or not, and `graphicle config show` lists the main
limits and every one that is set.

A value is a whole number from 1 to the largest value in the table. A size is
a number of bytes, or a number with `KiB`, `MiB`, `GiB` or `TiB`, such as
`2GiB`. A value that is not valid, or an unknown key under `limits`, stops the
command with a message that names the setting and the values it takes.

The source text a project keeps, earlier versions included, is four times the
text per project limit, at least 2 GiB; `storage.prune` removes earlier
versions.

**No single file stops a sync.** A file past a per-file limit is analysed as
far as the limit allows, and its text is still indexed and searchable. A text
file over the size of one text file is left out. A file whose analysis fails
for a reason of its own is skipped by the analysis, with the reason, and its
text is still searchable; a parse that did not finish is tried again by the
next sync. A file that cannot be read, and a document that cannot be
converted, are left out. The result of `sync` lists these files, each with its
path and the reason, under `limited` and `skipped`; the keys appear only when
there are such files. On a terminal, `graphicle sync` shows the first five of
each with the reasons and `graphicle setup` counts them in its summary; when
`graphicle sync` prints JSON, it also prints one line with their count on
standard error.
When you change an analysis limit, raised or lowered, the next sync analyses
again the files that were limited or skipped by analysis, and only those.
Files analysed in full are not analysed again, so a lowered limit applies to
them when they change.

A connector sync works the same way: a document past a size limit is listed
under `skipped` in the sync's result, with its reason, and the sync goes on
with the next document.

**What one question reads.** A question to a project with a passage index
(see [The passage index](#the-passage-index)) ranks every passage of every
file. Without one, and for the options that take the full path, one question
searches and ranks at most 50,000 documents and 40,000 passages of up to 2,600
characters (about 100 MB of text) from a project. A project within these
bounds is searched whole. In a larger project, the question chooses: the
documents that match more of its words are searched first. Before it chooses,
a question still reads the text of every document into memory: a 60,000-file
project used about 1.1 GiB per question when this was measured.
The answer's `coverage` then says `selected_by_question`, and counts the
documents left out. A sync of such a project without a passage index says so
in its result (`question_limits`), and in its readable output or on standard
error. To choose what a question reads,
pass `include` with path prefixes. `graph.query` answers with no nodes and
says `degraded` when the project's property graph would have more than 50,000
nodes or 200,000 edges, as a project of more than 50,000 documents does.

**Uploads.** An upload through the API or MCP (`documents.import`) carries
the document in its request: at most 24 MiB over HTTP and 16 MiB as an MCP or
command-line argument, base64 included, whatever `limits.document_bytes`
says. A larger document can be synced from a folder. The document converter
runs with at most 1 GiB of memory and 30 seconds of processor time per
document.

`graphicle sync` sends a large project in batches. The size of one request (at
most 5,000 manifest entries and 64 MiB of text) is internal and is never a
project limit.

`graphicle sync` respects `.gitignore`, `.git/info/exclude` and `.ignore`
files. The first two cover the files Git does not track: a file Git tracks is
read even when one of them matches it, since Git versions it with the rest (a
repository that ignores its build output `app.js` and tracks `src/app.js`),
and the sync's output counts those files (`tracked_ignored`). A file an
`.ignore` file leaves out stays out, tracked or not. `graphicle config set
sync.tracked_ignored false` (`"sync": {"tracked_ignored": false}` in
`models.json`) leaves tracked files that a pattern matches out too, and the
environment variable `GRAPHICLE_SYNC_TRACKED_IGNORED` (`0` or `1`) overrides
the setting. It skips
`.git`, `node_modules`, `target`, build output, `.env` files, keys and
credential files, symbolic links and binary files other than documents. It
reads PDF, DOCX, PPTX, XLSX, ODF and EPUB documents, HTML and UTF-8 text. It
also skips Graphicle's own writes: the `<file>.graphicle-bak` backups, Claude
Code's personal `.claude/settings.local.json`, a `.mcp.json` or
`.vscode/mcp.json` holding only Graphicle's entries, and the Graphicle section
of `AGENTS.md` and `CLAUDE.md`, which reads as blank lines. The output of
`sync` counts what it excluded. To make a project smaller, set up a subfolder
(`graphicle setup path/to/subfolder`), or leave untracked paths out in
`.gitignore` or, for your machine only, in `.git/info/exclude` (with
`sync.tracked_ignored` set to `false`, tracked paths too), or any path in an
`.ignore` file.

The code graph has no such limits.

## The passage index

Each sync keeps a passage index of the project's files in the store: the
passages `graphicle context` ranks, with the words, conversation turns and
identifiers in each. A sync reads only the files that changed since the last
one, and drops what it held of files that changed or went away. After each
code graph build, Graphicle also saves the graph and its search index in the
data folder (`gf-cache/<project>/query/`), so a question reads them instead of
rebuilding them from the store.

`graphicle context`, the memory server's `recall_project` and the prompt hook
then look a question up in the index. The ranking is the one the full path
gives over every passage of the project. These read the project the full way
instead: a call with `include` or `metadata`, `stem: false`, a `speaker` list,
`query_variants`, `key_expansion`, `note_keys`, `doc_links: true`,
`symbol_links: true`, `source_age: true`, `notes_first: true`,
`graph_retrieval: true` or a local model profile, and a project with reviewed
memory. So does a project whose index is missing (a store from an earlier
version builds it at its next sync) or lacks more than 2,000 of its current
files.

`graphicle doctor` says, under Questions, whether the index and the saved
graph are ready, and `graphicle setup` says so when they are not. The packet
reports what it read in `passage_index`. `GRAPHICLE_PASSAGE_INDEX=0` makes
every question take the full path, and `GRAPHICLE_SAVED_GRAPH=0` makes it
read the code graph from the store.

## Freshness

Before `ask`, `context`, `status` and each agent tool call, Graphicle checks
whether the project's folder changed since the last sync. When a few files
changed, it syncs them first; when many changed, it answers at once and says
how many files changed since the last sync. To turn the check off, run
`graphicle config set freshness off`; `graphicle config set freshness auto`
turns it back on (the default). The check applies to projects synced from a
folder. The environment variable `GRAPHICLE_FRESHNESS=auto|off` overrides
the setting for one command or shell.

In JSON output, a sync before the answer, or a stale project, is reported in
the `folder_freshness` field (the `freshness` field is the reviewed memory's).
The stdio server that `graphicle connect` writes checks before each tool
call, and so does the HTTP MCP endpoint of `graphicle serve`: there the sync
runs as the data folder's own session, for callers in its workspace who may
read the project. A sync of a folder whose files did not change converts no
document again: an unchanged PDF, Office file, page or image keeps the text
this version of Graphicle extracted from it.

`graphicle sync --quiet` returns at once when nothing changed. The Git hooks
that `graphicle setup` adds run it in the background after each commit, merge
and checkout.

## Terminal output

On a terminal, commands print readable text; in a pipe, they print JSON for
scripts. `--json` asks for JSON on a terminal, and `GRAPHICLE_OUTPUT=text` or
`GRAPHICLE_OUTPUT=json` chooses for every command. `NO_COLOR=1` turns colour
off. `GRAPHICLE_NO_BROWSER=1` makes `graphicle open` print the app's address
instead of starting a browser.

When a newer release is out, a command you run in a terminal ends with a
three-line notice on standard error: the new version, the upgrade command and
the release page. Graphicle asks GitHub for the latest release at most once a
day; a check that has no answer by the time a command ends counts as that
day's check. After a check that fails, or that GitHub refuses or limits, the
next one waits a day, then two days after a second failure in a row, then
four. It prints the notice at most once a day.
`GRAPHICLE_NO_UPDATE_NOTIFIER=1` or `graphicle config set update_notifier false`
turns both off.

## Code graph

| Setting | Effect |
|---|---|
| `.graphicle/` | The graph's output folder in the project. `GRAPHICLE_OUT` sets another one |
| `.graphicleignore` | Paths the graph leaves out, in `.gitignore` syntax. `--no-gitignore` stops `extract` from reading `.gitignore` |
| `~/.graphicle-graph/` | Engine settings: custom LLM providers (`providers.json`, managed with `graphicle graph provider add`), the global graph and grammar packs |
| `--backend NAME --model NAME` | The LLM that reads documents, papers and images in a full `extract` (code always goes through the parser). Without `--backend`, the engine takes the first key it finds: `GEMINI_API_KEY` or `GOOGLE_API_KEY`, `MOONSHOT_API_KEY`, `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `DEEPSEEK_API_KEY`, then Azure OpenAI (`AZURE_OPENAI_API_KEY` with `AZURE_OPENAI_ENDPOINT`) or an Ollama address (`OLLAMA_BASE_URL`). `--code-only` never calls an LLM, and `--dry-run` reports what a full extract would send |
| `--precise` or `GRAPHICLE_PRECISE=1` | Adds precise call resolution to `extract`, `update`, `watch` and `serve --watch` |
| `GRAPHICLE_SCIP` | SCIP index files whose calls `--precise` imports (default `<root>/index.scip`) |
| `--min-evidence LEVEL` or `GRAPHICLE_MIN_EVIDENCE` | Leaves out call edges below `name`, `binding`, `type`, `indexer` or `runtime` |
| `GRAPHICLE_QUERY_SNAPSHOT=0` | Turns off the saved query snapshots that speed up repeated queries |
| `GRAPHICLE_NO_AUTO_REFRESH=1` | Stops the engine from refreshing installed skills that an upgrade left behind |
| `GRAPHICLE_PROFILE=1` | Prints time and memory per stage of `extract`, `update` and `watch` |
