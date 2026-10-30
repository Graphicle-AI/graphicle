# Packs: one prompt of a repository's code

`graphicle pack` gathers a project's code for one question into a single
block of text that fits a token budget, laid out for a model with a long
context window. `graphicle ask --pack` sends such a pack with the question
to a model in one call, and `graphicle plan` asks for a structured plan of a
change. All three read the project Graphicle already synced: no new index.

```sh
graphicle pack "Where does the scheduler count matching pods for spread constraints?" > pack.md
graphicle pack "How are LimitRange checks done?" --budget 512000 --format xml --out pack.xml
graphicle pack --budget 32000                 # no question: an overview of the repository
graphicle ask --pack "Why does the Job controller count a failed pod create twice?"
graphicle plan "Add a metric for evicted pods to the taint manager"
```

## What a pack holds

A pack is built from Graphicle's own retrieval: the ranking `graphicle
context` uses (the passage index, the identifier arm and the code-graph arm),
read as a long ranked list of files instead of a few excerpts.

The ranking reads the question without an issue's environment sections (a
heading below the title that labels the system information, tested versions,
a `<name> version`, the environment, the operating system and the like, or an
issue form's question for them) and its `<name> version: <value>` lines: they
say where the reporter ran the program, not what the code does. A title or a
heading that merely names a version ("Upgrade to version 2 breaks login")
stays in the question. Right after
the top five files come the files the question names:
- by a code name: `TextEdit` names `text_edit.cpp` and `text_edit.h`,
  `FormControl` names `form_control.ts`, `UserModel` names `UserModel.java`;
- by two or three of its words, in the languages that name files after their
  classes (Java, TypeScript and JavaScript, C and C++): "the FileSystem dock"
  names `filesystem_dock.cpp`;
- by a folder name ("Alexa Devices" names `alexa_devices/`).

After the first 15 ranks come the files that define what the top files use:
the Go identifiers they take from imported packages, the Python modules they
import from their own folder, and the TypeScript and JavaScript modules they
import by a relative path or a tsconfig `paths` alias.

The saved code graph gives the line ranges of symbols, the call edges and the
tests. The sections, each with its share of the budget:

| Section | What it holds | Share |
|---|---|---|
| Header | The repository, commit, budget, tokens used, how the files were chosen and how tokens were counted | |
| Manifest | Every file in the pack with its tokens, section and rank, then the top-ranked files left out | |
| Architecture | One line per folder: files, tokens, main language, purpose (from a README or package comment), key symbols by connections in the code graph, entry points; then the dependencies between folders | about 4% (20% without a question) |
| Focus | The ranked files: small ones whole, large ones as excerpts of the symbols around the ranked passages, with overlapping ranges merged; in a standard pack of 32,768 tokens or less, the first ranked file whole when its estimated size (from its bytes) fits in half the budget, and in one of 131,072 tokens or more (so not at the default 128,000), the first two whole when each one's estimated size fits in a quarter of it; not in a pack with pinned passages (`fix --focus`, `review`), where every file keeps the caps | 50% or more |
| Call paths | Callers and callees of the focus symbols, two steps along call edges, as `a (path:line) -> b (path:line)`, then the bodies of the key ones | up to 10% |
| Tests | Tests that call or reference the focus symbols, from the code graph, else the test files that go with the top files by name | up to 10% |
| Docs | Ranked documentation passages and the nearest README of the top files | up to 5% |
| Breadth | The next ranked files, then files in the same folders as the top files, then the rest of the repository by connections in the code graph; a file too large to go in whole goes in as an outline (the first line of each symbol) | the rest |

Without a question, a pack is an overview: the architecture, then files by
connections in the code graph.

Every file chunk names its path, its line range and why it is in the pack.
In Markdown:

````text
### pkg/store/cache.go lines 40-95
why: focus: rank 1, defines Cache.Get, Cache.evict
```go
40	func (c *Cache) Get(key string) (Item, bool) {
...
```
````

and in XML:

```xml
<file path="pkg/store/cache.go" lines="40-95" why="focus: rank 1, defines Cache.Get, Cache.evict">
...
</file>
```

A file's lines go in as they are, not escaped. When they hold `</file` or
`</section` (an HTML page's own `</section>`), they go inside a CDATA section
that starts on a line of its own, so that the page never closes the pack's
`<file>` or `<section>`:

```xml
<file path="web/index.html" lines="1-40" why="focus: rank 1">
<![CDATA[
1	<!doctype html>
...
]]>
</file>
```

## Options

| Option | Default | Meaning |
|---|---|---|
| `QUESTION` | none | The question or change request; `-` reads it from standard input |
| `--project P` | this folder's project | A project id, key or synced folder |
| `--budget TOKENS` | 128000 | The pack's size, a hard limit, from 512 to 2,000,000 tokens |
| `--format` | `markdown` | `markdown`, `xml`, or `json` (below) |
| `--tokenizer PATH` | none | Count tokens exactly with a Hugging Face `tokenizer.json`, for example the tokenizer of the model you will send the pack to |
| `--bytes-per-token N` | none | Estimate tokens as UTF-8 bytes divided by N for all text alike, without a margin, to match another tool that sizes text that way |
| `--order` | `relevant-last` | `relevant-last`: header, manifest, architecture, breadth, docs, tests, call paths, then focus, so the most relevant code is next to the question that follows the pack. `relevant-first` puts focus first |
| `--line-numbers` | `focus` | Which chunks start each line with its number and a tab: `focus`, `all` or `none` |
| `--sections LIST` | all | Comma-separated: `arch`, `map`, `focus`, `paths`, `tests`, `docs`, `breadth`, `manifest` (`map` is the cache layout's) |
| `--layout` | `standard` | `standard`, or `cache`: a repository part that is the same for every question at a commit, then a question part ([below](#layouts-and-the-prompt-cache)) |
| `--prefix-share F` | 0.9 | With the cache layout, the share of the budget the repository part takes at most, from 0.1 to 1; 1.0 makes a stable pack. Alone, it chooses the cache layout |
| `--out FILE` | standard output | Write the pack to a file |

**Tokens.** With `--tokenizer`, every chunk and the whole pack are counted
with that tokenizer, and the budget is never exceeded. Without it, each
chunk is estimated as its UTF-8 bytes, less its line-number prefixes,
divided by the bytes per token of its file's language, plus 1.6 tokens per
line-number prefix (2.6 past line 9,999); 3% of the budget is held back.
The rates are the Muse Spark Open tokenizer's bytes per token on two large
repositories (kubernetes and home-assistant) over 1.03: for example 3.63
bytes per token for Go, 4.34 for Python, 3.8 for Markdown and 4.1 for JSON;
languages those repositories hardly hold keep lower rates. In 22 packs of
128,000 tokens from those repositories, the estimate ran 7% above that
tokenizer's count (2.5% to 11% per pack). Other tokenizers count
differently: pass `--tokenizer` for an exact count. `--bytes-per-token N`
counts every chunk as its UTF-8 bytes divided by N instead, without a
margin. The header says which way the pack was counted.

**The same inputs give the same pack.** The text is byte for byte the same
for the same project state, question and options.

**Speed.** A pack reads the ranking and the query file that `graphicle
sync` keeps, and file text from the store. On a shallow clone of kubernetes
(30,315 files) on 48 cores of a Linux server, building a pack for each of the
22 questions of GitHub issue #8 took a median of 1.8 seconds at 128,000,
512,000 and 1,000,000 tokens alike, with at most 1.0 GB of memory; the whole
command, which first checks the folder for changed files, took a median of
4.8 seconds. The query file belongs to one build of Graphicle: after an
upgrade, the first pack writes it once and says so (24 seconds on that
kubernetes clone, 16 on home-assistant), and later packs read it.

## Layouts and the prompt cache

A provider with a prompt cache bills the longest start of a prompt it has
seen recently at its cached price. The Meta Model API caches by itself: on
Muse Spark 1.3, cached input costs $0.15 instead of $1.25 per million
tokens; on Muse Wonder, $0.20 instead of $4 (list prices on 26 October
2026). A standard pack names the question in its header, so packs for two
questions share almost nothing. `--layout cache` lays a pack out so that
they share almost all of it:

- **The repository part** comes first and depends only on the project's
  files and the settings (budget, prefix share, tokenizer, format, order,
  line numbers and sections): a header without the question, the
  architecture summary, a map of the folders (the files directly in each,
  their tokens and its most connected symbols), the files with the most
  connections in the code graph (whole, or the first line of each symbol when
  large) and the top documentation (the README and architecture documents).
  It takes at most the prefix share of the budget (`--prefix-share`, 0.9 by
  default).
- **The question part** follows: a manifest, focus, call paths and tests for
  the question, in the room left. It never repeats lines the repository part
  holds; ranked files held whole there are listed as `in the prefix: path
  lines a-b`. It gives full excerpts to one ranked file per 8,000 tokens of
  its room (3 to 15 files, each up to two fifths of the room divided among
  them, or 2,000 tokens when that is more) and compact excerpts to the ranks
  after them.
- The question itself is not in the pack: it goes after it, as `ask --pack`,
  `plan` and `fix` send it.

`--prefix-share 1.0` makes a stable pack: the repository part alone, the same
for every question, so only the question after it changes. Two packs with the
same `prefix_sha256` share their repository part byte for byte. The header
names no budget. `ask --pack`, `plan` and `fix` without `--budget` size the
repository part from the fixed budget the model's input limit gives (the
input limit less 4,000 tokens), not from the budget the calibration sets
(below). What holds, exactly:

- While the calibration (the provider's count over Graphicle's estimate) is
  at most 0.97 divided by the prefix share (0.97 at `--prefix-share 1.0`,
  1.08 at 0.9), the repository part is sized from the fixed budget and stays
  the same bytes whatever the calibration.
- Above that, the calibrated budget cannot hold the share of the fixed
  budget, and the repository part is sized from 0.8 of the fixed budget, or
  0.64, 0.512 and so on: the largest of these steps whose share fits. It
  stays the same bytes while the calibration stays within that step (for
  the 0.8 step, at most 0.97 divided by 0.8 times the prefix share: 1.21 at
  `--prefix-share 1.0`, 1.35 at 0.9), and changes when the calibration
  crosses into another step.
- The first call of a model profile has no calibration yet and uses the
  fixed budget, so when the provider counts more than the estimate, the
  second call's repository part differs from the first's; from there on
  the steps above hold.

```sh
graphicle pack "Where does the scheduler count matching pods for spread constraints?" --layout cache --budget 1000000
graphicle ask --pack --layout cache "Why does the Job controller count a failed pod create twice?"
graphicle pack --prefix-share 1.0 --budget 1000000 --out stable.md
```

**Measured on kubernetes** (commit ca8a2a9e4, a shallow clone of 30,315
files; the 22 questions of GitHub issue #8, each with its answer's key file;
counted with Muse Spark Open's tokenizer; no model calls). "Key file in the
pack" counts the questions whose key file has any of its text in the pack:
whole, or as excerpts or outlines of its symbols. The cost of a
follow-up question is the repository part at the cached price plus the
question part at the input price, computed at list prices, not billed;
output, the instructions and the question are extra.

| Budget | Layout | Key file in the pack | Repository part (tokens) | Question part (tokens) | Follow-up, Muse Spark 1.3 | Follow-up, Muse Wonder |
|---|---|---|---|---|---|---|
| 256K | standard | 21 of 22 | 0 | 255,354 | $0.32 | $1.02 |
| 256K | cache 0.5 | 22 of 22 | 127,713 | 117,794 | $0.17 | $0.50 |
| 256K | cache 0.7 | 19 of 22 | 178,838 | 72,157 | $0.12 | $0.32 |
| 256K | cache 0.9 | 17 of 22 | 229,933 | 24,730 | $0.07 | $0.14 |
| 256K | cache 0.95 | 17 of 22 | 242,709 | 12,402 | $0.05 | $0.10 |
| 256K | stable (1.0) | 2 of 22 | 255,481 | 0 | $0.04 | $0.05 |
| 512K | standard | 22 of 22 | 0 | 510,860 | $0.64 | $2.04 |
| 512K | cache 0.5 | 21 of 22 | 255,464 | 227,507 | $0.32 | $0.96 |
| 512K | cache 0.7 | 22 of 22 | 357,662 | 139,480 | $0.23 | $0.63 |
| 512K | cache 0.9 | 18 of 22 | 459,860 | 49,097 | $0.13 | $0.29 |
| 512K | cache 0.95 | 17 of 22 | 485,384 | 24,840 | $0.10 | $0.20 |
| 512K | stable (1.0) | 2 of 22 | 510,963 | 0 | $0.08 | $0.10 |
| 1M | standard | 22 of 22 | 0 | 997,914 | $1.25 | $3.99 |
| 1M | cache 0.5 | 22 of 22 | 498,998 | 428,002 | $0.61 | $1.81 |
| 1M | cache 0.7 | 21 of 22 | 698,573 | 263,859 | $0.43 | $1.20 |
| 1M | cache 0.9 | 21 of 22 | 898,171 | 92,094 | $0.25 | $0.55 |
| 1M | cache 0.95 | 18 of 22 | 948,084 | 47,769 | $0.20 | $0.38 |
| 1M | stable (1.0) | 4 of 22 | 997,976 | 0 | $0.15 | $0.20 |

Token counts are the means over the 22 questions.

The repository part was the same bytes for all 22 questions in every row.
Counting with the tokenizer, building a pack took a median of 3.9 seconds at
256K, 4.6 at 512K and 6.1 at 1M on 48 cores of a Linux server, with at most
1.2 GB of memory (the first packs after a new build also write the
project's query file once, which took 34 seconds and 4.6 GB here).

The trade-off is the question part's size. At the default share it holds a
tenth of the budget, so a cache-layout pack covers fewer of the question's
ranked files than a standard pack of the same budget: at 0.9 the key file
was in the pack for 17, 18 and 21 of the 22 questions at 256K, 512K and 1M,
against 21, 22 and 22 for standard packs. A larger question part closes most
of that gap at a higher cost per follow-up (0.5: 22, 21 and 22). A stable
pack holds no question part at all. `ask --pack` and `plan` keep the
standard layout by default; choose the cache layout and its share when you
will ask several questions about the same commit.

## The JSON format

`--format json` prints one object. Its chunks carry the Markdown text, and
the pack's text is the chunks' `text` joined in order:

```json
{"format":"graphicle.pack.v1","project":"my-app","commit":"<git HEAD or null>","question":"...",
 "budget":128000,"tokens":N,"tokenizer":"estimate","order":"relevant-last",
 "layout":"cache","prefix_share":0.9,"prefix_tokens":N,"prefix_sha256":"<hex>",
 "sections":[{"name":"focus","part":"suffix","tokens":N,
   "chunks":[{"path":"pkg/x.go","start_line":10,"end_line":80,"whole":false,"tokens":N,
              "why":"focus: rank 3, defines Foo","text":"..."}]}],
 "included":[{"path":"pkg/x.go","tokens":N,"rank":3}],
 "omitted":[{"path":"pkg/y.go","rank":51,"tokens":N}],
 "seconds":N}
```

- `included` lists every file with at least one chunk; `rank` is its place
  in the retrieval's ranking, or `null` for files the breadth or
  architecture sections chose.
- `omitted` lists the top-ranked files left out, with estimated tokens.
- Chunks that are not file text (headings, the manifest, the architecture,
  the call paths) have `path`, `start_line` and `end_line` set to `null`.
- `layout` is `standard` or `cache`; `prefix_share` is there for the cache
  layout only. Every section has a `part`, `prefix` or `suffix`: the prefix
  sections come first, and their chunks' text joined in order (the prefix) is
  the start of the pack's text, byte for byte. `prefix_tokens` counts it, and
  `prefix_sha256` is the SHA-256 of its UTF-8 bytes. A standard pack for a
  question has no prefix (its header names the question): `prefix_tokens` is
  0 and the hash is that of the empty text. An overview is all prefix.

## From an agent: the `pack` tool

The MCP server publishes a `pack` tool in the whole server, which `graphicle
setup` gives Muse Code, Codex, Cursor and Claude Desktop, and in its memory
half (`--tools memory`). Claude Code's entry runs the five essential tools;
`graphicle connect claude-code --tools all` gives it `pack` too. It takes
`question` and `budget` (default 32,000 tokens, at most 200,000) and returns
the pack's Markdown, with the summary fields as structured content. Its
description tells an agent to call it once before a repository-wide question
or a change that spans files, instead of many searches and reads.

## `graphicle ask --pack`

```sh
graphicle ask --pack "Where is the loopback decision for NodePort services made?"
graphicle ask --pack --budget 256000 --model meta "..."
```

- The pack fills the model's input limit, less 4,000 tokens for the
  instructions and the question, or `--budget`. After each call Graphicle
  keeps, per model profile, the ratio of the provider's prompt tokens to its
  own estimate (`pack-calibration.json` in the data folder), and sizes the
  next pack without `--budget` so that the provider's count fills the input
  limit less 3%.
- One model call: the instructions ask for an answer from the pack only,
  with every claim cited as `path:line`, and for a plain statement of what
  the pack does not show.
- The answer streams to the terminal from chat models. Then every cited
  `path:line` is checked against the working tree (the file exists and the
  lines are inside it), and the citations that do not hold are listed.
- The last line gives the tokens in and out from the provider's usage
  counts, and a cost when the profile has a `price` (see
  [configuration](configuration.md#muse-spark)).
- `--model NAME` picks another configured profile; `--json` prints the
  answer, the checked citations, the usage, the prompt cache key and the
  pack's summary as JSON.
- `--layout cache` (and `--prefix-share F`) sends a cache-layout pack. The
  request starts with the same instructions for every question, then the
  pack, then the question; with the Meta Model API it carries a prompt cache
  key and a 24-hour retention, and a request near the input limit is first
  counted with the provider ([configuration](configuration.md#muse-spark)).
- Each call is added to the spend log, and the profile's cost caps apply
  ([configuration](configuration.md#prices-the-spend-log-and-caps)).
- The `basic` and `reflect` workflows are unchanged without `--pack`.

## `graphicle plan`

```sh
graphicle plan "Count evictions per taint in the taint manager and expose them as a metric"
graphicle plan - --json < request.txt
```

`plan` makes a pack focused on the change request and asks the model for a
plan as JSON (schema-constrained where the profile has structured output):

- a summary and the steps in order;
- the files to edit, each with a line range and the reason;
- new files, with their purpose;
- tests to add, update or run;
- risks.

Graphicle then checks the plan against the working tree: files to edit must
exist and their ranges must be inside them, new files must not exist yet,
and tests to update or run must exist. Items that do not hold are marked
`check`. The plan prints as Markdown, or as JSON with `--json`. `plan`
changes no files. Like `ask --pack`, it takes `--layout` and
`--prefix-share`, sends the prompt cache key to the Meta Model API, and adds
its call to the spend log. To make the change itself and test it on a new
branch, see [`graphicle fix`](fix.md).

## Which model

`ask --pack` and `plan` work with any configured chat model; a pack is only
as large as the model's input limit allows. For a whole-repository pack,
use a model with a long context window:

- Muse Spark or Muse Wonder through the Meta Model API: `graphicle config
  set model meta` or `graphicle config set model meta:muse-wonder-1.0`
  ([configuration](configuration.md#muse-spark));
- Muse Spark Open, or any model, on your own vLLM or SGLang server:
  `graphicle config set model compatible:MODEL --url http://HOST:8000/v1`,
  then raise `input_limit` to the server's context length.
