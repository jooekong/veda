#!/usr/bin/env bash
# One-shot driver: seed, then ramp every endpoint with metrics attribution.
# Logs land in $OUT (k6 summary + per-layer sample table per round).
#
# Prereqs: server up with VEDA_METRICS_TOKEN, `node provision.mjs` already run.
#   METRICS_TOKEN=<token> ./run_all.sh
#
# Rounds are ordered clean→heavy; the UNIQUE_TEXT=0/1 pairs isolate the real
# embedding cost. delete runs last (it consumes seed ids).
set -uo pipefail
cd "$(dirname "$0")"
source .env.loadtest
export METRICS_TOKEN="${METRICS_TOKEN:-loadtest-scrape-token}"

OUT="${OUT:-/tmp/loadtest}"
mkdir -p "$OUT"
SEED_TOTAL="${SEED_TOTAL:-20000}"

echo "[seed] filling $SEED_TOTAL records ..."
TOTAL="$SEED_TOTAL" node seed.mjs

# run <label> <op> <extra k6 -e flags...>
run() {
  local label="$1" op="$2"; shift 2
  echo "[run] $label ($op) ..."
  node sample_metrics.mjs --op "$op" --interval 2 > "$OUT/$label.sample.log" 2>&1 &
  local sp=$!
  k6 run -e BASE="$BASE" -e WK="$WK" -e OP="$op" "$@" k6_vectors.js > "$OUT/$label.k6.log" 2>&1
  kill "$sp" 2>/dev/null
  sleep 3
}

run 1_query           query    -e MAX_VUS=150 -e STEP_SECS=15 -e SEED_TOTAL="$SEED_TOTAL"
run 2_search_fulltext search   -e MODE=fulltext -e MAX_VUS=120 -e STEP_SECS=15
run 3_search_hybrid_iso  search -e MODE=hybrid   -e UNIQUE_TEXT=0 -e MAX_VUS=100 -e STEP_SECS=15
run 4_search_hybrid_real search -e MODE=hybrid   -e UNIQUE_TEXT=1 -e MAX_VUS=50  -e STEP_SECS=15
run 5_search_semantic_real search -e MODE=semantic -e UNIQUE_TEXT=1 -e MAX_VUS=50 -e STEP_SECS=15
run 6_upsert_iso      upsert   -e UNIQUE_TEXT=0 -e MAX_VUS=60 -e STEP_SECS=15
run 7_upsert_real     upsert   -e UNIQUE_TEXT=1 -e UPSERT_BATCH=20 -e MAX_VUS=40 -e STEP_SECS=15
run 8_delete          delete   -e MAX_VUS=100 -e STEP_SECS=15 -e SEED_TOTAL="$SEED_TOTAL"

echo "[done] logs in $OUT"
