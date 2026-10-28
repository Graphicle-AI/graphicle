#!/usr/bin/env bash
# Whether this run reviews: on the pull_request event only, and not for a
# pull request from a fork (it gets no secrets) or without a model key.
# Writes run=true or run=false to the step's outputs.
set -euo pipefail

skip() {
  echo "graphicle review: skipped: $1"
  echo "run=false" >> "$GITHUB_OUTPUT"
  exit 0
}

case "$EVENT" in
  pull_request) ;;
  pull_request_target)
    echo "::error::graphicle review refuses pull_request_target: that event runs with the repository's secrets and a write token, and this action reads the pull request's code. Use the pull_request event."
    exit 1
    ;;
  *)
    echo "::error::graphicle review runs on the pull_request event, not ${EVENT}."
    exit 1
    ;;
esac

if [ "$HEAD_REPO" != "$BASE_REPO" ]; then
  skip "the pull request comes from a fork (${HEAD_REPO:-deleted}), and GitHub gives fork pull requests no secrets. Review it locally with graphicle review."
fi
if [ "$HAS_KEY" != true ]; then
  skip "no model key. Add the MODEL_API_KEY secret and pass it as model-api-key."
fi
echo "run=true" >> "$GITHUB_OUTPUT"
