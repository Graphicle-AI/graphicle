# Code signing policy

**Not in effect yet.** Graphicle's Windows programs are not code-signed
today. We plan to have them signed through the SignPath Foundation, which
signs open-source projects for free, and this policy applies once our
application is approved. Until then, Windows releases are unsigned
([Windows signing](install.md#windows-signing)).

Once the application is approved:

> Free code signing provided by [SignPath.io](https://about.signpath.io), certificate by [SignPath Foundation](https://signpath.org)

## What is signed

- `graphicle.exe` and `graphicle-graph.exe` in the Windows release archive,
  `graphicle-<version>-x86_64-pc-windows-msvc.zip`. The macOS and Linux
  archives are not covered.
- Only programs that the [release workflow](../.github/workflows/release.yml)
  builds from this repository's source on GitHub-hosted runners. SignPath
  checks that the archive it signs comes from that workflow's run.
- Each release is signed only after an approver approves its signing request
  in SignPath. The signed archive replaces the unsigned one before the
  release's checksums (`SHA256SUMS` and the `.sha256` files) are written.

## Team roles

- Committers and reviewers: [Graphicle-AI maintainers](https://github.com/orgs/graphicle-ai/people)
- Approvers: [VikramLex](https://github.com/VikramLex)

A maintainer reviews every change from someone outside the team, such as a
pull request, before it is merged. The approver decides whether a release is
signed. Team members must use multi-factor authentication on GitHub and on
SignPath.

## Privacy policy

Graphicle has no telemetry and no account. It contacts the network only in
these cases:

- **The update check, which is on by default.** Once a day, when you run a
  command in a terminal, `graphicle` sends one GET request to GitHub's
  releases API (`api.github.com/repos/Graphicle-AI/graphicle/releases/latest`)
  to learn the latest version, and tells you when a newer one exists. If
  GitHub has not answered by the time the command ends, the request is sent
  again with your next command. The request carries no code, project data,
  version or credentials; like any web request, it shows GitHub your IP
  address. Agents' MCP servers and hooks, CI runs, and commands whose output
  is JSON or not a terminal never check. To turn it off, set
  `GRAPHICLE_NO_UPDATE_NOTIFIER=1` or run
  `graphicle config set update_notifier false`.
- **A model, only when you configure one.** The model you set for answers,
  memory proposals or embeddings receives the text it works on.
- **A connector, only when you configure one.** It reads from the service you
  connect.
- **A full `graphicle graph extract`** (without `--code-only`) sends
  documents, papers and images to the LLM whose API key it finds in your
  environment. `--dry-run` shows what it would send.
- **The GitHub CLI**, which `graphicle graph add`, `clone` and the
  pull-request tools run.
- **`graphicle update`** with a feed you name.
- **A build from source** downloads the parser sources once.

The services that Graphicle contacts for you have their own privacy policies:
[GitHub's](https://docs.github.com/site-policy/privacy-policies/github-general-privacy-statement)
for the update check and the GitHub CLI, and those of the model provider and
the services you connect.
