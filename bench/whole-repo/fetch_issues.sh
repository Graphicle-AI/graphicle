#!/usr/bin/env bash
# Fetch the linked issues that mine.py asks for, with their edit history and title changes.
#
#   bash fetch_issues.sh NEEDDIR OUTDIR
#
# NEEDDIR holds issues-needed-<name>.txt (one issue number per line, written by `mine.py check`).
# Writes OUTDIR/issues-<name>-<batch>.json, 20 issues per GraphQL call. Read-only calls through the
# GitHub CLI. The edit history gives the body as first filed (issue_text.py).
set -euo pipefail
NEED=${1:?usage: fetch_issues.sh NEEDDIR OUTDIR}
OUT=${2:?usage: fetch_issues.sh NEEDDIR OUTDIR}
mkdir -p "$OUT"
FIELDS='__typename
  ... on Issue {
    number url createdAt closedAt title body lastEditedAt
    userContentEdits(first: 100) { totalCount nodes { createdAt editedAt deletedAt diff } }
    timelineItems(itemTypes: [RENAMED_TITLE_EVENT], first: 50) {
      nodes { ... on RenamedTitleEvent { createdAt previousTitle currentTitle } }
    }
  }'
for pair in kubernetes:kubernetes/kubernetes home-assistant:home-assistant/core pytorch:pytorch/pytorch \
            vscode:microsoft/vscode elasticsearch:elastic/elasticsearch angular:angular/angular \
            keycloak:keycloak/keycloak godot:godotengine/godot; do
  name=${pair%%:*}; slug=${pair#*:}; owner=${slug%%/*}; repo=${slug#*/}
  list="$NEED/issues-needed-$name.txt"
  [ -s "$list" ] || continue
  batch=0; nums=()
  while read -r n || [ -n "$n" ]; do
    [ -n "$n" ] && nums+=("$n")
    if [ ${#nums[@]} -eq 20 ]; then
      batch=$((batch + 1)); f=$(printf '%s/issues-%s-%03d.json' "$OUT" "$name" "$batch")
      if [ ! -s "$f" ]; then
        q="query { repository(owner: \"$owner\", name: \"$repo\") {"
        for m in "${nums[@]}"; do q+=" i$m: issueOrPullRequest(number: $m) { $FIELDS }"; done
        gh api graphql -f query="$q } }" > "$f.part" && mv "$f.part" "$f"
      fi
      nums=()
    fi
  done < "$list"
  if [ ${#nums[@]} -gt 0 ]; then
    batch=$((batch + 1)); f=$(printf '%s/issues-%s-%03d.json' "$OUT" "$name" "$batch")
    if [ ! -s "$f" ]; then
      q="query { repository(owner: \"$owner\", name: \"$repo\") {"
      for m in "${nums[@]}"; do q+=" i$m: issueOrPullRequest(number: $m) { $FIELDS }"; done
      gh api graphql -f query="$q } }" > "$f.part" && mv "$f.part" "$f"
    fi
  fi
  echo "$name: $batch batches"
done
