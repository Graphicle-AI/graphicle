# Changelog

Notable changes to Graphicle: the graph engine (`graphicle-graph`), the memory
library (`graphicle-memory`) and the `graphicle` program.

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
