# `graphicle wiki`: a project wiki a model writes and keeps

`graphicle wiki build` asks a model to write a wiki of your codebase: one
article per area of the code and an index with a one-line summary per
article. Agents and people read the index and the articles instead of the
raw files, and the wiki costs nothing to read afterwards: no model is
called to read it. `graphicle wiki update` writes again only the articles
whose sources changed. `graphicle wiki check` finds what went stale in your
docs, your reviewed memories and the wiki, and can ask the model to propose
corrections for you to approve.

The idea comes from Andrej Karpathy's post "LLM Knowledge Bases" (2 April
2026): a model compiles raw sources into a Markdown wiki, keeps it current
and runs health checks over it. Graphicle applies it to a codebase: every
paragraph cites file and line, every model call is opt-in, capped and
logged, and nothing leaves your machine except the model calls you start.

```sh
graphicle wiki build --dry-run          # the articles to write and the worst case of their cost
graphicle wiki build --max-cost 2       # write the wiki, stopping before a call would pass 2 US dollars
graphicle wiki update                   # write again only what changed
graphicle wiki check --no-model         # health checks with no model
graphicle wiki check --max-cost 0.5     # and proposed corrections, for your approval
graphicle wiki hook on --max-cost 0.25  # update after each commit (off by default)
```

It needs a synced project (`graphicle sync .`), run in its folder so that
citations can be checked against the working tree, and a model profile, for
example `graphicle config set model meta` ([configuration](configuration.md)).
Any configured chat profile works, including one on this machine (Ollama or
another local server).

## What an article holds

The areas are the communities of the project's code graph, the same
clustering the graph tools read, largest first. An area needs at least four
code symbols; a smaller community joins the area it has the most graph edges
with, and one with none is left out and counted in the index.
`--max-articles` (default 30) caps the number of articles: an area that
already has an article keeps it while the area exists, and new areas are
added, largest first, until the cap. The wiki keeps `--max-articles` and
`--budget` in its manifest, so later runs that name neither, such as the
commit hook's, cover the same areas with the same budget.

For each area the model gets:

- a code pack ([packs](pack.md)) whose focus is the area's most connected
  symbols, within `--budget` tokens (default 12,000);
- the area's files, its most connected symbols and the areas it connects to,
  by graph edges between them;
- the passages of the project's own documents that name the area's code
  (repository files only, never the wiki's own articles), each marked when
  the code it names changed after it was written;
- the reviewed, current memories linked to the area's symbols or files
  (retained answers among them).

Captured agent sessions and connector documents are never sent: they are not
reviewed and may hold private text.

The model returns a title, a one-line summary, and paragraphs on what the
area does, its key parts, how it connects to other areas, and what the docs
and memories say, each paragraph with the `path:line` citations it rests on,
plus open questions. Graphicle checks every citation: it must fall in the
lines the request showed the model (the pack's excerpts and the doc
passages) and those lines must exist in the working tree. A citation that
does not hold is dropped, and a paragraph left with none is dropped. The run
reports both counts. The check says that the cited lines were in front of
the model, not that they prove the paragraph. Text from the model is
escaped so that it cannot add Markdown links, images or HTML to the page (a
viewer such as GitHub may still turn a bare `https://` address into a
link).

Each article starts with its provenance: the model, the profile, the time,
the commit and a digest of its sources. A line under the title says a model
wrote it. A last section, "Listed by Graphicle", is not written by the
model: the connected areas (with links to their articles), the area's files,
the docs that name its code and the reviewed memories about it.

## Where the wiki lives

By default the wiki is `.graphicle/wiki/written/` in the project's folder:
`written/index.md` and one `<area>.md` per area, with `manifest.json`, the
record `wiki update` and `wiki check` read. The agent rules Graphicle
installs already point agents to `.graphicle/wiki/index.md`; when that file
does not exist, `wiki build` writes a short one that points to
`written/index.md`. The engine's `graphicle graph export wiki`, which writes
an article per community with no model, replaces the `.md` files at the top
of `.graphicle/wiki/` and never touches `written/`, so the two do not
overwrite each other. The export does replace that short
`.graphicle/wiki/index.md` with its own index, which does not point to
`written/`: open `written/index.md` directly, or delete the export's
`index.md` and run `graphicle wiki update` to write the pointer again.

`--out DIR` writes the wiki elsewhere, for example `docs/wiki` to keep it in
the repository. In a folder that `graphicle sync` reads, the articles are
also sources of recall: an excerpt of an article carries the note "written
by a model (graphicle wiki): read the code it cites before relying on it"
and comes after the code and docs. The articles are never sources of the
wiki itself: they are not doc passages of an area, they do not close a
knowledge gap, and they are never listed as stale docs. The wiki's
`manifest.json` and `proposals/` are not areas of the code either.
`.graphicle/` itself is not synced.

Each article and the index start with front matter that marks them as
written by `graphicle wiki`. A file in the wiki's folder without it, such as
your own `index.md` or a file with an article's name, is never replaced or
removed: the run reports it and goes on. One run at a time writes a wiki
folder: a second run that finds `.graphicle-wiki.lock` held stops.

## Keeping it current

`graphicle wiki update` computes each area's sources again (its files'
contents, the doc passages and the memories) and compares their digest with
the one each article was written from. It writes again only the articles
whose sources changed and removes the articles of areas that are gone.
Every other article keeps the text the model wrote; only its "Listed by
Graphicle" section is rendered again from the graph, so it keeps its bytes
unless, for example, its connected areas changed. An area keeps its article's name
while it shares at least half of its files with it, so a new clustering
does not rename articles. Each article's files are also recorded as its
sources in Graphicle's dependency index, so a sync marks the article stale.

`wiki build` keeps finished articles too: a run that stops, at `--max-cost`
or because you stopped it, goes on where it ended the next time.
`--force` writes every article again.

## What it costs

Before each call Graphicle works out its worst case: the request's input
tokens (the pack and the area's text, estimated, times the profile's
calibration from earlier calls, plus a 10% margin), none of them cached, and
the output limit, at the profile's configured price. The output limit is
4,000 tokens an article, or 16,000 for a profile that sets
`reasoning_effort`, since reasoning tokens count as output; never more than
the profile's own `output_limit`. A model that reasons by default without
`reasoning_effort` set still gets 4,000 output tokens, which its reasoning
can use up: set `reasoning_effort` for such a model. The run stops before a
call whose worst case would bring it over `--max-cost` (default 1 US
dollar). A finished call counts at the usage the provider reported, or at
its worst case when the provider reported none (not every
OpenAI-compatible server does). A call that fails is counted too: at its
cost in the spend log when the provider reported usage (a reply cut at the
output limit is billed), else at its worst case. The input side of the
worst case is an estimate: a call whose input was much larger than
estimated can cost more than its worst case and bring the run past
`--max-cost` by that difference; it is counted at what it cost, and the
run then stops before the next call. Three failed calls in a row stop the run. The profile's own
caps (`max_cost_per_call`, `max_cost_per_day`) apply too, and every call
goes in the spend log (`graphicle usage`), without prompts or answers.
`--dry-run` prints the articles to write and the worst case of their cost,
by the same formula, and calls nothing. A profile without a price runs only
when its endpoint is on this machine (`localhost` or a loopback address).

As a rough guide, an article's request is about `--budget` tokens plus a few
thousand for the area's text; at 12,000 tokens of pack and a price of 1.25
US dollars per million input tokens and 4.25 per million output tokens, one
article's worst case is about 0.04 US dollars. Use `--dry-run` for the
figure with your profile and project.

## Health checks

`graphicle wiki check` runs these checks with no model:

- **Stale doc mentions**: a passage of a document that names a function or
  class whose code changed after the passage last changed, or that no longer
  exists (renamed or deleted). A mention that resolves to several
  definitions is not flagged. The same flags show in `graph.doc_links`
  (`stale` on each mention, and `stale: true` to list them) and in recall,
  as one note on the excerpt.
- **Stale memories**: reviewed memories withheld because the code or sources
  they cite changed.
- **Articles whose sources changed**, articles of areas that are gone, and
  articles missing from the folder.
- **Broken citations**: `path:line` citations in the articles that no longer
  hold in the working tree.
- **Knowledge gaps**: areas of the code with no doc passage, reviewed memory
  or article about them.

Without `--no-model`, a model pass follows, under `--max-cost` (default 1 US
dollar) and at most `--max-items` (default 20) stale doc mentions and as
many stale memories. For each, the model reads the passage or the memory and
the current code it names, and answers that it still holds, that it is
outdated (with a corrected text and the code lines it cites), or that it
cannot tell. A passage that names several changed definitions is one call.
Only files whose bytes are the ones last synced are read and sent: a file
the sync leaves out, or one changed since, is skipped (sync first). The
pass counts failed calls as `build` does. The results are proposals;
Graphicle never changes your files:

- a doc's correction is written as a patch in `proposals/` in the wiki's
  folder, which you can read and apply with `git apply` (it keeps CRLF line
  ends and a last line without a line end; a later check replaces the patch
  for the same passage);
- a memory's correction becomes a draft memory in the approval queue of
  `graphicle open`, citing the code lines it rests on, for a member to
  review.

A correction that cites no code line that holds, among the lines its
request showed, is not kept. One more call
suggests articles and questions for the knowledge gaps. There is no web
search.

`--json` prints the report as JSON for scripts.

## The commit hook

`graphicle wiki hook on --max-cost 0.25` adds a block to the repository's
post-commit hook that runs `graphicle wiki update` in the background after
each commit, with that cap per run: ten commits can spend ten times as
much. To cap a day, set the profile's `max_cost_per_day`
([configuration](configuration.md)). A run that finds another one writing
the wiki stops. It is off until you turn it on, and `graphicle wiki hook
off` removes the block; the rest of the hook is kept, and a hook left with
nothing else is removed. The block follows the rules of the freshness hooks
that `graphicle setup` adds: it goes before a final `exit` or `exec` line,
and a hook in another language (Python, for one) is left alone, with the
line to add yourself.

## What is sent to the model

For `build` and `update`: the code pack, the area's files and symbols, the
doc passages that name its code and the reviewed memories about it, as
above. For `check`: each stale passage or memory with the current code it
names, and the files and symbol names of the areas with no doc. Nothing else
from the project, and nothing from captured sessions or connectors.
