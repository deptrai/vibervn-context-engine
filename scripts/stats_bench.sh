#!/usr/bin/env bash
# Index-stats endpoint latency oracle — measures GET /api/repos/:repo_id/index-stats.
#
# WHY: that endpoint runs THREE full-table `count() ... GROUP ALL` scans
# (file_meta, chunk, symbol) on EVERY request. On the real already-indexed Linux
# kernel repo (~79k files, ~909k chunks, ~2.6M symbols) one request currently
# takes ~15s. This script is the pass/fail oracle for the upcoming persisted-cache
# fix — it does NOT change any server code, it only measures per-request
# wall-clock latency against the REAL running server and emits a
# machine-readable evidence artifact.
#
# LOCKED GATE: PASS if p50 < 100ms, else FAIL. Printed as `RESULT: PASS|FAIL`.
#
# Usage:
#   scripts/stats_bench.sh <label> [iterations] [repo_id] [port]
# Args / env:
#   <label>       required, e.g. baseline | optimized  (names the artifact)
#   [iterations]  default 20; or env ITERATIONS. Baseline is ~15s/request on the
#                 current no-cache path, so 20 iterations of baseline ≈ 5 min —
#                 that is expected; the gate must be measured at a real sample
#                 size. The optimized path is milliseconds so 20 is trivial.
#   [repo_id]     default = base64(c:\users\0x317\downloads\linux) = the indexed
#                 Linux kernel repo; or env REPO_ID.
#   [port]        default 6699; or env PORT / CONTEXT_ENGINE_PORT.
#
# Output:
#   stdout  human-readable summary + RESULT verdict
#   file    context-engine-rs/bench-results/stats/<label>.json  (evidence)
# Exit code:
#   0  RESULT: PASS and zero failed requests
#   1  RESULT: FAIL, or any non-200 / invalid-body request, or preflight error
#
# Deps: bash + curl + coreutils (sort) + awk. No Rust binary, no jq.
set -euo pipefail

LABEL="${1:?usage: stats_bench.sh <label> [iterations] [repo_id] [port]}"
ITERATIONS="${2:-${ITERATIONS:-20}}"
# Default repo_id = urlsafe-base64(no-pad) of "c:\users\0x317\downloads\linux".
DEFAULT_REPO_ID="YzpcdXNlcnNcMHgzMTdcZG93bmxvYWRzXGxpbnV4"
REPO_ID="${3:-${REPO_ID:-$DEFAULT_REPO_ID}}"
PORT="${4:-${PORT:-${CONTEXT_ENGINE_PORT:-6699}}}"

URL="http://127.0.0.1:${PORT}"
STATS_URL="${URL}/api/repos/${REPO_ID}/index-stats"

# Artifact dir is anchored to the crate root (this script lives in scripts/), so
# it survives any CWD and any temp cleanup.
CRATE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${CRATE_DIR}/bench-results/stats"
OUT_JSON="${OUT_DIR}/${LABEL}.json"
mkdir -p "$OUT_DIR"

# Scratch files for response bodies (cleaned on exit).
TMP_BODY="$(mktemp)"
WARM_BODY="$(mktemp)"
cleanup() { rm -f "$TMP_BODY" "$WARM_BODY" 2>/dev/null || true; }
trap cleanup EXIT

echo "[stats_bench] label=${LABEL} repo_id=${REPO_ID} port=${PORT} iterations=${ITERATIONS}"
echo "[stats_bench] target=${STATS_URL}"

# ── Preflight: server reachable? ─────────────────────────────────────────────
# /api/index-status is a cheap always-present GET (see server.rs route table).
# We do NOT rebuild/re-index here — the repo is assumed already indexed (a kernel
# rebuild takes hours). If the server is down, tell the operator how to start it.
if ! curl -fsS -m 10 "${URL}/api/index-status" >/dev/null 2>&1; then
  echo "ERROR: server not reachable at ${URL}" >&2
  echo "       Start it first, e.g. from context-engine-rs/:" >&2
  echo "         LIBCLANG_PATH='C:\\Program Files\\LLVM\\bin' cargo run -r" >&2
  echo "       (or: just rs-dev). The Linux repo must already be indexed." >&2
  exit 1
fi

# ── Validate one /index-stats response: HTTP 200 + JSON with a \"chunks\" key. ─
# Echoes:  "<http_code> <time_total_seconds>"  on success path (caller checks
# the code). Body validation: must literally contain a "chunks" key. We treat a
# non-200 or a body without "chunks" as a FAILURE, never as a latency sample.
fetch_stats() {  # fetch_stats <body_file> -> prints "<code> <time_total>"
  local body="$1" code_time code tt
  # -sS: quiet but show errors; -o body; -w code+time; -m 300: 5min hard cap so a
  # hung request can't wedge the harness. Don't use -f (we want to capture non-200).
  code_time="$(curl -sS -m 300 -o "$body" -w '%{http_code} %{time_total}' "$STATS_URL" 2>/dev/null || echo '000 0')"
  printf '%s' "$code_time"
}

body_has_chunks() {  # body_has_chunks <body_file> -> rc 0 if valid
  grep -q '"chunks"' "$1" 2>/dev/null
}

# ── Warm-up (NOT counted) — avoids charging cold connection setup to sample 1. ─
# But: the current no-cache path is slow on EVERY request, so a slow warm-up is
# itself signal. We record + print its latency separately.
echo "[stats_bench] warm-up request (not counted) ..."
WARM_CT="$(fetch_stats "$WARM_BODY")"
WARM_CODE="${WARM_CT%% *}"
WARM_SEC="${WARM_CT##* }"
WARM_MS="$(awk -v s="$WARM_SEC" 'BEGIN{printf "%.1f", s*1000}')"
if [ "$WARM_CODE" != "200" ] || ! body_has_chunks "$WARM_BODY"; then
  echo "ERROR: warm-up request failed (http=${WARM_CODE}, body invalid)." >&2
  echo "       First 200 bytes of body:" >&2
  head -c 200 "$WARM_BODY" >&2; echo >&2
  exit 1
fi
echo "[stats_bench] warm-up: http=${WARM_CODE} latency=${WARM_MS}ms (this is the per-request cost on the no-cache path)"

# Index-size fields from the warm-up body (cheap structural grep: the integer
# after each \"files\":, \"chunks\":, \"symbols\": key). These describe the index
# size; identical across requests for a static index. Best-effort — extraction
# misses must NOT fail the run.
FILES_COUNT="$(grep -o '"files"[[:space:]]*:[[:space:]]*[0-9]*' "$WARM_BODY" | head -1 | grep -o '[0-9]*$' || true)"
CHUNKS_COUNT="$(grep -o '"chunks"[[:space:]]*:[[:space:]]*[0-9]*' "$WARM_BODY" | head -1 | grep -o '[0-9]*$' || true)"
SYMBOLS_COUNT="$(grep -o '"symbols"[[:space:]]*:[[:space:]]*[0-9]*' "$WARM_BODY" | head -1 | grep -o '[0-9]*$' || true)"

# ── Measured iterations ──────────────────────────────────────────────────────
SAMPLES_MS=()        # successful per-request latencies (ms)
FAIL_COUNT=0
declare -A CODE_COUNTS=()   # http_code -> count (all attempts incl. failures)

for i in $(seq 1 "$ITERATIONS"); do
  CT="$(fetch_stats "$TMP_BODY")"
  CODE="${CT%% *}"
  SEC="${CT##* }"
  CODE_COUNTS["$CODE"]=$(( ${CODE_COUNTS["$CODE"]:-0} + 1 ))
  if [ "$CODE" = "200" ] && body_has_chunks "$TMP_BODY"; then
    MS="$(awk -v s="$SEC" 'BEGIN{printf "%.1f", s*1000}')"
    SAMPLES_MS+=("$MS")
    echo "[stats_bench] iter ${i}/${ITERATIONS}: http=${CODE} ${MS}ms"
  else
    FAIL_COUNT=$((FAIL_COUNT + 1))
    echo "[stats_bench] iter ${i}/${ITERATIONS}: FAILURE http=${CODE} (body invalid?) — not counted in latency stats" >&2
    head -c 160 "$TMP_BODY" >&2; echo >&2
  fi
done

N_OK="${#SAMPLES_MS[@]}"
if [ "$N_OK" -eq 0 ]; then
  echo "ERROR: zero successful samples — every request failed. Cannot compute latency." >&2
  exit 1
fi

# ── Percentiles: nearest-rank on the sorted sample set. ──────────────────────
# Method (documented): sort ascending; for percentile p, rank = ceil(p/100 * N)
# clamped to [1, N]; value = sample at that 1-based rank. min = rank 1,
# max = rank N. This is deterministic and needs no interpolation.
SORTED="$(printf '%s\n' "${SAMPLES_MS[@]}" | sort -n)"
STATS="$(printf '%s\n' "$SORTED" | awk '
  { a[NR]=$1 }
  END {
    n=NR
    rank_p50=int(0.50*n); if (0.50*n > rank_p50) rank_p50++; if (rank_p50<1) rank_p50=1; if (rank_p50>n) rank_p50=n
    rank_p95=int(0.95*n); if (0.95*n > rank_p95) rank_p95++; if (rank_p95<1) rank_p95=1; if (rank_p95>n) rank_p95=n
    printf "%.1f %.1f %.1f %.1f", a[1], a[rank_p50], a[rank_p95], a[n]
  }')"
MIN_MS="$(echo "$STATS" | awk '{print $1}')"
P50_MS="$(echo "$STATS" | awk '{print $2}')"
P95_MS="$(echo "$STATS" | awk '{print $3}')"
MAX_MS="$(echo "$STATS" | awk '{print $4}')"

GATE_MS=100
# Verdict: PASS iff p50 < gate. awk returns 1(true)/0(false).
PASS_GATE="$(awk -v p="$P50_MS" -v g="$GATE_MS" 'BEGIN{print (p < g) ? 1 : 0}')"
if [ "$PASS_GATE" = "1" ] && [ "$FAIL_COUNT" -eq 0 ]; then
  RESULT="PASS"
else
  RESULT="FAIL"
fi

TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# ── Human-readable summary ───────────────────────────────────────────────────
echo
echo "==================== stats_bench summary ===================="
echo " label        : ${LABEL}"
echo " timestamp    : ${TS}"
echo " repo_id      : ${REPO_ID}"
echo " endpoint     : ${STATS_URL}"
echo " iterations   : ${ITERATIONS}  (ok=${N_OK}, failed=${FAIL_COUNT})"
echo " warm-up      : http=${WARM_CODE} ${WARM_MS}ms (not counted)"
echo " latency (ms) : min=${MIN_MS}  p50=${P50_MS}  p95=${P95_MS}  max=${MAX_MS}"
echo " percentile   : nearest-rank, rank=ceil(p/100*N) on ascending sort"
echo " index size   : files=${FILES_COUNT:-?} chunks=${CHUNKS_COUNT:-?} symbols=${SYMBOLS_COUNT:-?}"
printf " http codes   :"; for c in "${!CODE_COUNTS[@]}"; do printf " %s=%s" "$c" "${CODE_COUNTS[$c]}"; done; echo
echo " gate         : p50 < ${GATE_MS}ms"
echo " RESULT: ${RESULT}   (p50=${P50_MS}ms p95=${P95_MS}ms vs gate ${GATE_MS}ms)"
echo "============================================================="

# ── Machine-readable evidence artifact ───────────────────────────────────────
# Hand-built JSON (no jq dependency). Samples are numbers; build the array.
SAMPLES_JSON="$(printf '%s\n' "${SAMPLES_MS[@]}" | awk 'NR>1{printf ","} {printf "%s",$1} END{print ""}')"
# http_status_counts object.
HTTP_JSON=""
for c in "${!CODE_COUNTS[@]}"; do
  [ -n "$HTTP_JSON" ] && HTTP_JSON="${HTTP_JSON},"
  HTTP_JSON="${HTTP_JSON}\"${c}\": ${CODE_COUNTS[$c]}"
done

cat > "$OUT_JSON" <<EOF
{
  "label": "${LABEL}",
  "timestamp": "${TS}",
  "repo_id": "${REPO_ID}",
  "endpoint": "${STATS_URL}",
  "iterations": ${ITERATIONS},
  "ok_samples": ${N_OK},
  "failed_requests": ${FAIL_COUNT},
  "warmup_ms": ${WARM_MS},
  "warmup_http": ${WARM_CODE},
  "percentile_method": "nearest-rank: rank=ceil(p/100*N) on ascending sort, 1-based",
  "samples_ms": [${SAMPLES_JSON}],
  "min_ms": ${MIN_MS},
  "p50_ms": ${P50_MS},
  "p95_ms": ${P95_MS},
  "max_ms": ${MAX_MS},
  "http_status_counts": { ${HTTP_JSON} },
  "files": ${FILES_COUNT:-null},
  "chunks": ${CHUNKS_COUNT:-null},
  "symbols": ${SYMBOLS_COUNT:-null},
  "gate_ms": ${GATE_MS},
  "result": "${RESULT}"
}
EOF
echo "[stats_bench] artifact -> ${OUT_JSON}"

# ── Exit code (CI-usable) ────────────────────────────────────────────────────
if [ "$RESULT" = "PASS" ]; then
  exit 0
else
  exit 1
fi
