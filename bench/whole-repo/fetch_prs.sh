#!/usr/bin/env bash
# Fetch the candidate pull requests of the benchmark's repositories from GitHub's GraphQL API.
#
#   bash fetch_prs.sh OUTDIR [NAME ...]
#
# With NAMEs, only those repositories are fetched.
# Needs the GitHub CLI, logged in (read-only calls). Writes OUTDIR/prs-<name>[-<part>].json: the raw
# search pages, concatenated as gh prints them. mine.py reads them.
#
# Candidates: pull requests merged into the default branch from the snapshot cutoff to the end of
# the window (repos.json) that GitHub links to an issue they close. PyTorch lands most pull requests
# with its merge bot, which closes them with the label "Merged" instead of merging them, and its
# stacked pull requests do not target the default branch, so GitHub records no closing link for
# them: for PyTorch every landed pull request is fetched, with its body, and mine.py reads the
# closing keywords (Fixes, Closes, Resolves #N) itself.
set -euo pipefail
OUT=${1:?usage: fetch_prs.sh OUTDIR [NAME ...]}
shift
ONLY=" $* "
mkdir -p "$OUT"
WIN='2026-09-14T00:00:00Z..2026-10-25T23:59:59Z'
QUERY='query($q: String!, $endCursor: String) {
  search(query: $q, type: ISSUE, first: 25, after: $endCursor) {
    issueCount
    pageInfo { hasNextPage endCursor }
    nodes {
      ... on PullRequest {
        number url title createdAt mergedAt closedAt merged state
        baseRefName baseRefOid headRefOid
        mergeCommit { oid }
        labels(first: 30) { nodes { name } }
        changedFiles
        files(first: 100) { totalCount nodes { path changeType additions deletions } }
        body
        closingIssuesReferences(first: 10) { nodes { number repository { nameWithOwner } } }
      }
    }
  }
}'
fetch() {  # name part search-query
  if [ "$ONLY" != "  " ] && [[ "$ONLY" != *" $1 "* ]]; then return; fi
  local f="$OUT/prs-$1$2.json"
  if [ -s "$f" ]; then echo "$f exists"; return; fi
  gh api graphql --paginate -f query="$QUERY" -f q="$3" > "$f.part"
  mv "$f.part" "$f"
  echo "$f: $(wc -c < "$f") bytes"
}
fetch kubernetes "" "repo:kubernetes/kubernetes is:pr is:merged base:master merged:$WIN linked:issue"
fetch home-assistant "" "repo:home-assistant/core is:pr is:merged base:dev merged:$WIN linked:issue"
fetch vscode "" "repo:microsoft/vscode is:pr is:merged base:main merged:$WIN linked:issue"
fetch elasticsearch "" "repo:elastic/elasticsearch is:pr is:merged base:main merged:$WIN linked:issue"
fetch pytorch "-landed" "repo:pytorch/pytorch is:pr is:closed label:Merged closed:$WIN"
fetch pytorch "-merged" "repo:pytorch/pytorch is:pr is:merged base:main merged:$WIN"
fetch angular "" "repo:angular/angular is:pr is:merged base:main merged:$WIN linked:issue"
fetch keycloak "" "repo:keycloak/keycloak is:pr is:merged base:main merged:$WIN linked:issue"
fetch godot "" "repo:godotengine/godot is:pr is:merged base:master merged:$WIN linked:issue"
