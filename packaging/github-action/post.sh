#!/usr/bin/env bash
# Post the review as one pull-request review, always a plain COMMENT: it never
# approves or requests changes. GitHub refuses the whole review when one of
# its comments sits on a line outside the pull request's diff; the comments
# then go into the review's body instead, still as one review.
set -euo pipefail

review="${RUNNER_TEMP}/graphicle-review.json"
if ! [ -s "$review" ]; then
  echo "graphicle review: no review to post."
  exit 0
fi
count=$(jq '.comments | length' "$review")
if [ "$count" = 0 ] && [ "$POST_EMPTY" != true ]; then
  echo "graphicle review: no inline comments, so nothing was posted."
  exit 0
fi

payload="${RUNNER_TEMP}/graphicle-review-post.json"
jq '.event = "COMMENT"' "$review" > "$payload"
endpoint="repos/${REPOSITORY}/pulls/${PULL}/reviews"
if ! gh api --method POST "$endpoint" --input "$payload" > /dev/null; then
  echo "::warning::GitHub refused the inline comments; posting them in the review's body instead."
  jq '{commit_id, event: "COMMENT", comments: [],
       body: (.body + "\n\n" + ([.comments[] | "- `\(.path):\(.line)`: \(.body | gsub("\n+"; " "))"] | join("\n")))}' \
    "$review" > "$payload"
  gh api --method POST "$endpoint" --input "$payload" > /dev/null
fi
jq -r '.body' "$review" >> "$GITHUB_STEP_SUMMARY"
echo "graphicle review: posted one review with ${count} inline comment(s)."
