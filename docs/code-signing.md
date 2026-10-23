# Code signing policy

> Free code signing provided by [SignPath.io](https://about.signpath.io), certificate by [SignPath Foundation](https://signpath.org)

**Release builds are not yet signed with a trusted certificate.** The release
workflow has the Windows programs signed through SignPath with a test
certificate. Windows does not trust that certificate, so it treats the
programs as unsigned and may warn before it runs them
([Windows signing](install.md#windows-signing)). Signing with the SignPath
Foundation's certificate, which Windows trusts, is planned. Each release's
notes say whether its Windows programs carry the test signature or are
unsigned.

## What is signed

- `graphicle.exe` and `graphicle-graph.exe` in the Windows release archive,
  `graphicle-<version>-x86_64-pc-windows-msvc.zip`. The Windows MCP bundle,
  which is built from that archive, holds the same programs. The macOS and
  Linux archives are not covered.
- Both programs carry version information: product name Graphicle, company
  KarLex AI, Inc., a file description, and the release's version.
- Only programs that the [release workflow](../.github/workflows/release.yml)
  builds from this repository's source on GitHub-hosted runners for a version
  tag. SignPath checks that the archive it signs comes from that workflow's
  run.
- Nothing is signed without the approver: GitHub holds each release's signing
  job until the approver approves it.
- The signed archive must hold the same files as the unsigned one, with only
  the two programs changed. It then replaces the unsigned archive before the
  release's checksums (`SHA256SUMS` and the `.sha256` files) are written.
- Signing never holds a release back. When it fails, times out or is not
  approved, the release ships the unsigned archive, and its notes say so.

## Team roles

- Committers: the [Graphicle-AI maintainers](https://github.com/orgs/Graphicle-AI/people),
  who may change the source code without further review.
- Reviewers: the same maintainers. They review every change from someone who
  is not a committer, such as a pull request, before it is merged.
- Approver: [VikramLex](https://github.com/VikramLex), who approves each
  signing request and whom the team trusts to decide whether a release is
  signed.

Team members must use multi-factor authentication on GitHub and on SignPath.

## Privacy policy

Graphicle has no telemetry, no crash reporting and no account. It contacts
the network only in the cases below. Apart from the update check, each one
happens only when you configure it or run the command that does it.

**The update check, which is on by default.** At most once a day, when you
run one of the `graphicle` commands that show their results in a terminal
(such as `graphicle`, `status`, `ask` or `doctor`), `graphicle` sends one GET
request to GitHub's releases API
(`api.github.com/repos/Graphicle-AI/graphicle/releases/latest`) to learn the
latest version, and tells you when a newer one exists. The request carries no
code, project data, version or credentials; like any web request, it shows
GitHub your IP address. A check that has no answer by the time the command
ends counts as that day's check; after a check that fails, or that GitHub
refuses or limits, the next one waits a day, then two days after a second
failure in a row, then four. Agents' MCP servers and hooks, CI runs, commands
whose output is JSON or not a terminal, and the `graphicle graph` engine never
check. To turn it off, set `GRAPHICLE_NO_UPDATE_NOTIFIER=1` or run
`graphicle config set update_notifier false`. The install scripts and the
Homebrew formula say this when they install Graphicle.

**Services you configure:**

- **A model.** The model you set for answers, memory proposals or embeddings
  (a provider such as Anthropic or OpenAI, or Ollama) receives the text it
  works on: your question with the source excerpts found for it, the texts
  to embed, and, when a connector's `observe` policy is on, what that
  connector reads. `graphicle setup` offers to set a model when it finds an
  Anthropic or OpenAI API key or a local Ollama.
- **A connector.** A connector you add (GitHub, Slack, Notion, Google Drive,
  Jira or Confluence) reads from that service with the credential you give
  it, and syncs again on the schedule you set. Signing in with OAuth
  exchanges codes and tokens with the provider you configure.
- **Shared storage.** A Neo4j graph database you configure receives the
  graph's nodes and edges and the text of graph searches; a Qdrant vector
  database receives vectors with source identifiers, but no text; a Redis or
  Valkey queue receives the model jobs it carries, with their instructions
  and input text.

**Commands that use the network:**

- **Extraction.** A full `graphicle graph extract` (or `graphicle graph
  <path>`) without `--code-only` sends documents, papers and images to the
  model backend whose API key it finds in your environment, or to the one
  `--backend` names; with no backend it stops and sends nothing. `--dry-run`
  sends nothing and shows what a run would send and an estimate of its cost.
  `--dedup-llm` sends node labels to the backend.
- **Community labels.** `graphicle graph label`, and `graphicle graph
  cluster-only` without `--no-label`, send the names of each community's
  members (symbols and files) to that backend, or to Claude Code (`claude
  -p`) when no API key is set and Claude Code is installed.
- **Web pages and repositories.** `graphicle graph add <url>` fetches the page
  you name; posts on X go through publish.twitter.com, arXiv papers through
  export.arxiv.org and YouTube videos through `yt-dlp`. Its requests name the
  Graphicle version in their User-Agent. `graphicle graph clone` runs `git`
  against GitHub.
- **Pull requests.** `graphicle graph prs`, `graphicle mcp --pull-requests`
  and the pull-request tools of `graphicle graph serve` run the GitHub CLI
  (`gh`) with your login. `--triage` also sends each pull request's number,
  title, author, check and review status and graph impact to a model backend.
- **Graph databases.** `graphicle graph export neo4j` or `falkordb` with
  `--push` sends the whole graph to the database you name.
- **Google Workspace.** `--google-workspace` runs the `gws` CLI with your login
  to export Google Drive files.
- **HTML views.** `graph.html` and the engine's other HTML views load their
  scripts from unpkg.com, d3js.org and cdn.jsdelivr.net when you open them in
  a browser. `--no-viz` skips `graph.html`.
- **Updates from a feed.** `graphicle update --feed <url>` reads the release
  manifest from the HTTPS feed you name and, with `--output`, downloads the
  installer it lists; it never runs that installer. Without `--feed`,
  `graphicle update` uses no network.

**Installing and building:** the install scripts and Homebrew download the
release from GitHub. A build from source downloads Rust crates from crates.io
and the parser sources from GitHub, pinned by SHA-256
([Build from source](install.md#build-from-source)).

**Servers:** the local web app (`graphicle serve` and `graphicle open`)
listens on this computer only, and `graphicle mcp` talks over standard input
and output. `graphicle graph serve --transport http` listens on 127.0.0.1
unless `--host` names another address.

The services that Graphicle contacts for you have their own privacy policies:
[GitHub's](https://docs.github.com/site-policy/privacy-policies/github-general-privacy-statement)
for the update check, the downloads and the GitHub CLI, and those of the
model providers, services, websites and script hosts you use through it.
