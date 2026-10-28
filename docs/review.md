# `graphicle review`: review a change with the repository's context

`graphicle review` asks a long-context model, Muse Spark by default with the
`meta` profile, to review a change: what a branch adds since its merge base
with the base branch, as a pull request shows it. Along with the diff, the
model gets what a reviewer would look up: the changed code, the
callers and callees of the changed symbols, the tests that touch them, and
the architecture of the modules the change touches. It returns comments on
changed lines, each with the `path:line` evidence it relies on, and Graphicle
checks every comment before you see it.

```sh
graphicle review                                  # what HEAD adds since the default branch
graphicle review --base main --max-comments 5
graphicle review --base origin/main --format json > review.json
graphicle review --base "$BASE_SHA" --format github > review.json   # for GitHub's review API
```

It needs a synced project (`graphicle sync .`, after you check out the head)
and a model profile, for example `graphicle config set model meta` with your
key in `MODEL_API_KEY` ([configuration](configuration.md#muse-spark)). It
changes no files and posts nothing. To post reviews on pull requests, use
[the GitHub Action](#the-github-action).

## Options

| Option | Default | Meaning |
|---|---|---|
| `--base REF` | `origin/HEAD`, else `origin/main`, `origin/master`, `main` or `master` | The branch, tag or commit to review against. The review covers what the head adds since their merge base. |
| `--head REF` | `HEAD` | The head. It must be the commit checked out in the project's folder, because the pack reads the synced files. |
| `--format` | `markdown` at a terminal, else `json` | `markdown`, `json` (the report) or `github` (the body of GitHub's create-a-review call). |
| `--max-comments N` | 20 | Comments to keep at most, from 0 to 100, the most severe first. |
| `--budget TOKENS` | the model's input limit less room for the diff | The pack's size. |
| `--model NAME` | the answer model | A configured model profile. |
| `--layout`, `--prefix-share` | `cache`, 0.9 | The pack's layout ([packs](pack.md#layouts-and-the-prompt-cache)). |
| `--project` | this folder's project | An id, a key or a synced folder. |

## What the model reads

One request, in this order:

1. **The instructions**, the same for every review.
2. **The repository part of the pack**: the architecture summary, a map of
   the folders and the most connected files. It is the same bytes for every
   review of the same commit at the same budget, so a second review of the
   same head (a re-run) can read it from the provider's prompt cache.
3. **The change part of the pack.** The changed hunks are pinned at the top
   of the pack's ranking, so its focus is the changed code (small files
   whole, large ones as the symbols the hunks touch), its call paths are the
   callers and callees of the changed symbols along the code graph's call
   edges (two steps at most), and its tests are the tests that reach them.
   The question's own ranking fills the rest.
4. **The change's text**: the architecture lines of the modules the change
   touches and the dependencies between them, the changed hunks with the
   line ranges a comment may name, and the diff. A diff over 400,000 bytes is
   cut; the list of hunks still names every file.

The diff comes from `git diff` between the merge base and the head, run with
hooks off, without external diff or text conversion drivers, and without the
variables that would point git at another repository. In a project that is a
subfolder of its repository, only the subfolder's changes are reviewed.

## What comes back, and what is checked

The model returns a summary and comments as structured output. Each comment
has a path, a line, a severity (`blocker`, `major`, `minor` or `nit`), a body
and its evidence: `path:line` or `path:start-end` citations. Graphicle sorts
each comment into one of three lists:

- **Comments**: on a line of the head inside one of the diff's changed hunks,
  with at least one citation that holds in the working tree (the file exists
  and has those lines). These are shown and, in the GitHub format, posted.
- **Flagged**: on a changed line, but no citation holds. Shown with the
  reason, never posted.
- **Dropped**: on a file the change does not add or modify, outside the
  changed hunks, empty, a repeat, or past `--max-comments`. Listed with the
  reason.

Citations that do not hold are marked on each comment. A review is advice
from a model: Graphicle checks where a comment points and what it cites, not
whether it is right.

## Output

- **markdown**: the summary, the three lists, and the call's usage line
  (tokens in, cached, out, and the cost at the configured price).
- **json**: `result` (`reviewed`; `no changes`, or `nothing to review` when
  the change only deletes, renames or changes binary files, both without a
  model call), `base`, `merge_base`,
  `head`, `files` (path, status, lines added and removed, hunk ranges),
  `summary`, `comments`, `flagged`, `dropped`, `model`, `usage`,
  `prompt_cache_key`, `context` (the changed symbols, the call-path lines, the
  callers' and callees' excerpts, the tests) and `pack` (the pack's summary).
- **github**: `{"commit_id", "event": "COMMENT", "body", "comments"}`, ready
  for `POST /repos/{owner}/{repo}/pulls/{number}/reviews`. Paths start at
  the repository's root, comments are on the head's side of the diff, and
  `@` before a name gets a word joiner, so the model's text never mentions
  anyone. The event is always `COMMENT`: the review never approves or
  requests changes.

Each review is one model call. It goes through the spend log and its caps
like `ask --pack` ([configuration](configuration.md)): `graphicle usage`
lists it, and `max_cost_per_call` refuses it before it is sent when its worst
case costs more.

## The GitHub Action

A composite action in this repository,
[`packaging/github-action/`](../packaging/github-action/action.yml), runs
`graphicle review --format github` on a pull request and posts one review.

```yaml
name: Graphicle review
on:
  pull_request:
    types: [opened, synchronize, reopened, ready_for_review]

permissions:
  contents: read
  pull-requests: write

concurrency:
  group: graphicle-review-${{ github.event.pull_request.number }}
  cancel-in-progress: true

jobs:
  review:
    if: github.event.pull_request.draft == false
    runs-on: ubuntu-24.04
    timeout-minutes: 30
    steps:
      - uses: graphicle-ai/graphicle/packaging/github-action@FULL_COMMIT_SHA # vX.Y.Z
        with:
          graphicle-version: X.Y.Z
          graphicle-sha256: SHA256_OF_THE_LINUX_X86_64_ARCHIVE
          model-api-key: ${{ secrets.MODEL_API_KEY }}
          max-cost-usd: "0.50"
```

Replace `X.Y.Z` with a release that has `graphicle review`, `FULL_COMMIT_SHA`
with that release tag's commit, and the SHA-256 with the line for
`graphicle-X.Y.Z-x86_64-unknown-linux-gnu.tar.gz` in the release's
`SHA256SUMS`. Add your Meta Model API key as the repository secret
`MODEL_API_KEY`.

The action:

1. checks the event: it runs on `pull_request` only, and skips with a message
   for a pull request from a fork or a run without a key;
2. checks out the pull request's head with its full history, without keeping
   the token in `.git`;
3. downloads the pinned release and checks its SHA-256 before unpacking it;
4. restores the index from the cache (`actions/cache`, keyed on the base
   commit), then syncs it to the head;
5. writes a `meta` profile with the price and the cost cap, and runs
   `graphicle review --format github`;
6. posts one review with the job's token. If GitHub refuses an inline
   comment (a line outside its view of the diff), the comments go into the
   review's body instead.

| Input | Default | Meaning |
|---|---|---|
| `graphicle-version` | required | The release to install. |
| `graphicle-sha256` | required | The SHA-256 of that release's archive for the runner. |
| `model-api-key` | empty: skip | The key; pass `secrets.MODEL_API_KEY`. |
| `model` | `muse-spark-1.3` | Or `muse-wonder-1.0` (set its `price` too). |
| `price` | `{"input":1.25,"output":4.25,"cached_input":0.15}` | US dollars per million tokens: Muse Spark 1.3's list price on 26 October 2026. |
| `max-cost-usd` | `0.50` | The most one review may cost at that price, worst case. |
| `budget` | `200000` | The pack's budget in tokens. |
| `max-comments` | `10` | Inline comments at most. |
| `post-without-comments` | `true` | Post the summary when there are no inline comments. |
| `github-token` | the job's token | The token that posts the review. |

The cap counts the worst case: the whole input at the uncached price plus
the output limit (16,000 tokens in the action) at the output price, with a
tenth added to an estimated input. At the defaults the pack and the output
come to about $0.34 (220,000 × $1.25 + 16,000 × $4.25 per million), plus the
diff's tokens at the input price: under the $0.50 cap. A larger `budget` needs a larger `max-cost-usd`, or the
call is refused before it is sent.

## Security

The action takes strict defaults and refuses the risky choices:

- **The `pull_request` event only.** On `pull_request`, GitHub gives a pull
  request from a fork no secrets and a read-only token, and the action skips
  it with a message: review it locally with `graphicle review`. The action
  refuses `pull_request_target`, which runs with the repository's secrets
  and a write token; never combine that event with a checkout of the pull
  request's code.
- **Least privilege.** The workflow grants `contents: read` and
  `pull-requests: write`, nothing else. The checkout keeps no credentials in
  `.git`. Only the review step sees the model key, and only the posting step
  sees the GitHub token: Graphicle never gets the token.
- **Pinned code.** The actions the action uses are pinned by full commit SHA,
  the workflow pins the action itself the same way, and the Graphicle release
  is pinned by version and SHA-256. Nothing runs from a moving tag.
- **One key, one place.** The model key comes only from the `MODEL_API_KEY`
  secret through the `model-api-key` input. Graphicle reads it from the
  environment and never writes it to the index, the cache or the log.
- **A cost cap per run.** One model call per run, refused before it is sent
  when its worst case passes `max-cost-usd`; `graphicle usage` prints the
  run's spend log at the end of the job. For a ceiling across runs, set a
  spend limit on the key itself.
- **Little network.** The release download from GitHub, the model API, and
  the GitHub API that posts the review. `graphicle sync` reads the checkout
  locally, and the new-release check is off on CI.
- **The model's text is untrusted.** A pull request's code and text reach
  the model, so a pull request can try to steer what the review says. The
  output can only become one `COMMENT` review on the pull request: comments
  sit on changed lines only, never approve or request changes, and mention
  no one. Treat the review as advice.
- **The cache.** GitHub scopes caches by branch: a pull request's run can
  read the base branch's caches and its own, and other branches cannot read
  what it saves. The index holds the repository's own code.
- **Where the code goes.** The pull request's code goes to the Meta Model
  API in Standard mode, which Meta does not use for training.
  `graphicle config set model` refuses the `-contributor` models, which Meta
  may train on.
