#!/usr/bin/env bash
# Configure the Meta profile with its price and cost cap, bring the index up
# to the pull request's head, and write the review for GitHub. The model key
# reaches Graphicle through MODEL_API_KEY in this step's environment only.
set -euo pipefail

case "$MODEL" in
  muse-spark-1.3 | muse-wonder-1.0) ;;
  *)
    echo "::error::model must be muse-spark-1.3 or muse-wonder-1.0, not '${MODEL}'."
    exit 1
    ;;
esac
if ! [[ "$MAX_COST" =~ ^[0-9]+(\.[0-9]+)?$ ]]; then
  echo "::error::max-cost-usd must be a number of US dollars, such as 0.50."
  exit 1
fi
if ! [[ "$BUDGET" =~ ^[0-9]+$ ]] || ! [[ "$MAX_COMMENTS" =~ ^[0-9]+$ ]]; then
  echo "::error::budget and max-comments must be whole numbers."
  exit 1
fi

export GRAPHICLE_NO_UPDATE_NOTIFIER=1
data="${RUNNER_TEMP}/graphicle-data"
config="${RUNNER_TEMP}/graphicle-models.json"
review="${RUNNER_TEMP}/graphicle-review.json"
graphicle_() { graphicle --data-dir "$data" --config "$config" "$@"; }

graphicle_ config set model "meta:${MODEL}" > /dev/null
graphicle_ config set models.meta.price "$PRICE" > /dev/null
graphicle_ config set models.meta.output_limit 16000 > /dev/null
graphicle_ config set models.meta.max_cost_per_call "$MAX_COST" > /dev/null

graphicle_ sync . --quiet
graphicle_ review --base "$BASE_SHA" --head "$HEAD_SHA" --format github \
  --budget "$BUDGET" --max-comments "$MAX_COMMENTS" > "$review"

echo "comments=$(jq '.comments | length' "$review")" >> "$GITHUB_OUTPUT"
graphicle_ usage || true
