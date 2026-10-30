#!/usr/bin/env bash
# The model runs, one step per call, all through one spend log with caps.
#
#   WORK=... BUILD=... TOK=... KEY_FILE=/path/to/0600-key [TEXTS=...] bash live.sh STEP
#
# STEP: e2 (noctx, then BM25 at 64K, then the pack at 64K; Standard) | c4c (the cache session; Standard) |
#       g1-pilot (at most 3 kubernetes questions per G1c arm, $0.14 per arm; Contributor) |
#       g1 (the G1c arms in their registered order; Contributor; record the build before the first call)
# WORK: the folder node_setup.sh fills. BUILD: the name of the Graphicle build in WORK/bin (graphicle-BUILD),
# such as its release version. TOK: the Muse Spark Open tokenizer.json. E1RUN: the E1 run whose saved contexts
# E2 uses (default e1). TEXTS (c4c, g1-pilot, g1): the whole-repository benchmark's question texts.
# TOOL_SEARCH=1 runs the agent arms with Meta's tool search. The runs go at nice 19; CPUS=LIST also pins them to
# those CPUs (taskset -c LIST).
# The key is read from KEY_FILE by the runners and never printed.
set -euo pipefail
: "${WORK:?set WORK to the folder node_setup.sh fills}"
: "${BUILD:?set BUILD to the name of the Graphicle build in WORK/bin, such as its release version}"
: "${TOK:?set TOK to the Muse Spark Open tokenizer.json}"
E1RUN=${E1RUN:-e1}
: "${KEY_FILE:?set KEY_FILE to the key file path}"
[ -r "$KEY_FILE" ] || { echo "KEY_FILE is not readable" >&2; exit 2; }
CODE=$WORK/code
PY=$WORK/venv/bin/python
LOG=$WORK/spend/spend-log.jsonl
BIN=$WORK/bin/graphicle-$BUILD
RUN="nice -n 19"
if [ -n "${CPUS:-}" ]; then RUN="taskset -c $CPUS $RUN"; fi
mkdir -p "$WORK/spend" "$WORK/out"

e2() {
  for arm in noctx bm25 pack; do
    $RUN "$PY" "$CODE/swe-bench-pro/e2.py" --work "$WORK" --arm "$arm" --out "$WORK/out/e2/$arm" \
      --e1-run "$E1RUN" --key-file "$KEY_FILE" --tokenizer "$TOK" --spend-log "$LOG" --spend-cap auto --jobs 4
  done
  $RUN "$PY" "$CODE/whole-repo/score.py" --runs "$WORK/out/e2/noctx" "$WORK/out/e2/bm25" "$WORK/out/e2/pack" \
    --out "$WORK/out/e2/score.json"
}

c4c() {
  : "${TEXTS:?set TEXTS to the question texts of the whole-repository benchmark}"
  $RUN "$PY" "$CODE/swe-bench-pro/c4c.py" run --work "$WORK" --out "$WORK/out/c4c" --texts "$TEXTS" \
    --key-file "$KEY_FILE" --tokenizer "$TOK" --spend-log "$LOG" --spend-cap auto
}

g1_arm() {  # g1_arm OUT ARM [BUDGET] [extra run.py options]
  local out=$1 arm=$2 budget=${3:-}
  : "${TEXTS:?set TEXTS to the question texts of the whole-repository benchmark}"
  shift 3 || shift $#
  local b=(); [ -n "$budget" ] && b=(--budget "$budget")
  local ts=(); case "$arm" in agent|agent-pack) [ "${TOOL_SEARCH:-0}" = 1 ] && ts=(--tool-search) ;; esac
  $RUN "$PY" "$CODE/whole-repo/run.py" --arm "$arm" "${b[@]}" --work "$WORK/g1-$BUILD" \
    --questions "$CODE/whole-repo/questions" --texts "$TEXTS" --out "$out" --profile meta \
    --model muse-spark-1.3-contributor --key-file "$KEY_FILE" --spend-log "$LOG" --tokenizer "$TOK" \
    --graphicle "$BIN" --redact "${ts[@]}" "$@"
}

g1_pilot() {
  local d=$WORK/g1-$BUILD/runs/pilot
  g1_arm "$d/noctx" noctx "" --repos kubernetes --limit 3 --spend-cap 0.14
  g1_arm "$d/trunc" trunc 128K --repos kubernetes --limit 3 --spend-cap 0.14
  g1_arm "$d/pack" pack 128K --repos kubernetes --limit 3 --spend-cap 0.14
  g1_arm "$d/pack-cache" pack-cache 128K --repos kubernetes --limit 3 --spend-cap 0.14
  g1_arm "$d/bm25" bm25 128K --repos kubernetes --limit 3 --spend-cap 0.14
  g1_arm "$d/agent" agent "" --repos kubernetes --limit 3 --spend-cap 0.14
  g1_arm "$d/agent-pack" agent-pack 32K --repos kubernetes --limit 3 --spend-cap 0.14
}

g1() {
  local d=$WORK/g1-$BUILD/runs/g1c h=pytorch,vscode,elasticsearch
  g1_arm "$d/noctx" noctx "" --repos "$h" --spend-cap auto
  g1_arm "$d/trunc" trunc 128K --repos "$h" --spend-cap auto
  g1_arm "$d/pack" pack 128K --repos "$h" --spend-cap auto
  g1_arm "$d/pack-cache" pack-cache 128K --repos "$h" --spend-cap auto
  g1_arm "$d/bm25" bm25 128K --repos "$h" --spend-cap auto
  g1_arm "$d/agent" agent "" --repos "$h" --spend-cap auto
  g1_arm "$d/agent-pack" agent-pack 32K --repos "$h" --spend-cap auto
}

case "${1:-}" in
  e2) e2 ;;
  c4c) c4c ;;
  g1-pilot) g1_pilot ;;
  g1) g1 ;;
  *) echo "usage: WORK=... BUILD=... TOK=... KEY_FILE=... [TEXTS=...] bash live.sh e2|c4c|g1-pilot|g1 (TEXTS for c4c, g1-pilot and g1)" >&2; exit 2 ;;
esac
