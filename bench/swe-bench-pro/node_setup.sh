#!/usr/bin/env bash
# Setup for the SWE-bench Pro runs. Re-runnable: each step skips what already exists.
#
#   WORK=... BUILD=... bash node_setup.sh STEP...      STEP: venv | data | binary | clones | all | g1
#
# WORK holds everything: venv/, raw/ (the pinned dataset file), tasks.jsonl, texts/ (problem statements),
# bin/ (the Graphicle build), mirror/ (one bare clone per repository) and code/ (a copy of this folder and
# ../whole-repo). BUILD names the Graphicle build, such as its release version (0.1.9). The binary step copies
# GRAPHICLE, a `graphicle` program built with `cargo build --release --locked --features local-inference`, to
# WORK/bin/graphicle-BUILD. The g1 step reads BENCH, the whole-repository benchmark's work folder (src/, manifest/
# and mine/), and TOK, the Muse Spark Open tokenizer.json its manifests were counted with. The steps run at
# nice 19; CPUS=LIST also pins them to those CPUs (taskset -c LIST). UV names the uv program (default uv).
set -euo pipefail
: "${WORK:?set WORK to the work folder}"
: "${BUILD:?set BUILD to a name for the Graphicle build, such as its release version}"
CODE=$WORK/code/swe-bench-pro
PY=$WORK/venv/bin/python
RUN="nice -n 19"
if [ -n "${CPUS:-}" ]; then RUN="taskset -c $CPUS $RUN"; fi
UV=${UV:-uv}
mkdir -p "$WORK/logs"

step_venv() {
  [ -x "$PY" ] && "$PY" -c 'import pyarrow, tokenizers, numpy' 2>/dev/null && { echo "venv: ok"; return; }
  "$UV" venv -q --python 3.12 "$WORK/venv"
  VIRTUAL_ENV=$WORK/venv "$UV" pip install -q pyarrow==26.0.0 tokenizers==0.23.3 numpy==2.5.4
  echo "venv: built"
}

step_data() {
  [ -s "$WORK/tasks.jsonl" ] && { echo "data: ok"; return; }
  $RUN "$PY" "$CODE/data.py" fetch --out "$WORK/raw"
  $RUN "$PY" "$CODE/data.py" tasks --parquet "$WORK/raw/test-00000-of-00001.parquet" \
    --tasks "$WORK/tasks.jsonl" --texts "$WORK/texts"
}

step_binary() {
  local bin=$WORK/bin/graphicle-$BUILD
  [ -x "$bin" ] && { echo "binary: ok ($("$bin" --version))"; return; }
  : "${GRAPHICLE:?set GRAPHICLE to a graphicle program built with --features local-inference}"
  mkdir -p "$WORK/bin"
  cp "$GRAPHICLE" "$bin.tmp"
  mv "$bin.tmp" "$bin"
  "$bin" --version
}

clone_one() {
  local repo=$1 dest=$WORK/mirror/${1/\//__}.git
  if [ -d "$dest" ]; then echo "clone $repo: ok"; return; fi
  $RUN git clone -q --bare "https://github.com/$repo.git" "$dest.tmp" && mv "$dest.tmp" "$dest"
  echo "clone $repo: done"
}

step_clones() {
  mkdir -p "$WORK/mirror"
  "$PY" -c 'import json,sys; print("\n".join(sorted({json.loads(l)["repo"] for l in open(sys.argv[1])})))' \
    "$WORK/tasks.jsonl" > "$WORK/repos.txt"
  local pids=()
  while read -r repo; do clone_one "$repo" & pids+=($!); done < "$WORK/repos.txt"
  local fail=0
  for p in "${pids[@]}"; do wait "$p" || fail=1; done
  [ "$fail" = 0 ] || { echo "clones: some failed" >&2; return 1; }
  # Every base commit must be in its mirror; fetch the missing ones by sha.
  "$PY" -c 'import json,sys; [print(t["repo"], t["base_commit"]) for t in map(json.loads, open(sys.argv[1]))]' \
    "$WORK/tasks.jsonl" | while read -r repo sha; do
      local m=$WORK/mirror/${repo/\//__}.git
      git -C "$m" cat-file -e "$sha^{commit}" 2>/dev/null || $RUN git -C "$m" fetch -q origin "$sha" || echo "missing $repo $sha"
    done
  echo "clones: ok"
}

# G1 (the whole-repository benchmark) and the cache meter: WORK/g1-BUILD holds the shared checkouts at S (read only,
# by symlink), their manifests (counted with the Muse tokenizer) copied, and indexes built by this build.
step_g1() {
  : "${BENCH:?set BENCH to the work folder of the whole-repository benchmark}"
  : "${TOK:?set TOK to the Muse Spark Open tokenizer.json}"
  local g1=$WORK/g1-$BUILD bench=$BENCH
  # tokens.py names an exact counter by the tokenizer's folder and file name.
  local want="exact:$(basename "$(cd "$(dirname "$TOK")" && pwd)")/$(basename "$TOK")"
  mkdir -p "$g1/src" "$g1/manifest" "$g1/data"
  for n in kubernetes pytorch vscode elasticsearch; do
    ln -sfn "$bench/src/$n" "$g1/src/$n"
    head -1 "$bench/manifest/$n.tsv" | grep -qF "$want" \
      || { echo "g1: the $n manifest was not counted with $TOK" >&2; return 1; }
    cp --update=none "$bench/manifest/$n.tsv" "$g1/manifest/$n.tsv"
  ln -sfn "$bench/mine" "$g1/mine"
  done
  $RUN "$PY" "$WORK/code/whole-repo/index.py" --binary "$WORK/bin/graphicle-$BUILD" --work "$g1" \
    kubernetes pytorch vscode elasticsearch
}

for s in "$@"; do
  case "$s" in
    venv) step_venv ;;
    data) step_data ;;
    binary) step_binary ;;
    clones) step_clones ;;
    g1) step_g1 ;;
    all) step_venv; step_data; step_binary; step_clones ;;
    *) echo "unknown step $s" >&2; exit 2 ;;
  esac
done
