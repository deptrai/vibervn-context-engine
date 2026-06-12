#!/usr/bin/env bash
# Reproducible A/B retrieval benchmark — legacy (pre-cAST) vs new (cAST) chunker.
#
# WHY: the archived baseline.json retrieval row was measured on the OLD on-disk
# index, which the cAST rebuild then OVERWROTE — so it was neither reproducible
# nor apples-to-apples. This script fixes that by Design B (dual-server via git
# worktree):
#
#   * legacy server  = a context-engine built from the submodule's git HEAD
#                      (the pre-cAST sliding-window chunker), on an isolated port
#                      + temp data dir.
#   * new server     = the working-tree (cAST) build, on its own port + temp dir.
#
# Both servers read the SAME home-anchored settings.json (so the embedding cache
# + API keys + repo list are shared — re-embeds are cheap since most chunks are
# already cached) but write to DISTINCT temp RocksDB data dirs, so NEITHER touches
# the currently-running :6699 index. We trigger a fresh full rebuild of the repo
# on each, wait for both to finish, then run the IDENTICAL harness + query path
# against each via `chunk_bench --ab`. Ground-truth is chunker-independent (frozen
# parse_file symbol extraction, unchanged vs HEAD), so the comparison is fair.
#
# Output: ab_benchmark.json with both rows side by side + deltas + run metadata.
#
# Usage:
#   scripts/ab_bench.sh [repo_path] [out_json]
# Defaults:
#   repo_path = d:/projects/cpp/notepad-ade
#   out_json  = ab_benchmark.json (in CWD)
set -euo pipefail

REPO="${1:-d:/projects/cpp/notepad-ade}"
OUT="${2:-ab_benchmark.json}"

# Resolve absolute output path before any cd.
OUT_ABS="$(cd "$(dirname "$OUT")" 2>/dev/null && pwd)/$(basename "$OUT")" || OUT_ABS="$PWD/$OUT"

export LIBCLANG_PATH="${LIBCLANG_PATH:-/c/Program Files/LLVM/bin}"

# Repo paths (this script lives in context-engine-rs/scripts/).
NEW_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LEGACY_WT="${LEGACY_WT:-D:/projects/Python/ce-legacy-wt}"

LEGACY_PORT="${LEGACY_PORT:-7801}"
NEW_PORT="${NEW_PORT:-7802}"
LEGACY_URL="http://127.0.0.1:${LEGACY_PORT}"
NEW_URL="http://127.0.0.1:${NEW_PORT}"

# Isolated temp data dirs (RocksDB only — embedding cache stays home-anchored).
# Use a Windows-native path (not an MSYS /tmp path) so the native binary can use
# it directly as --data-dir.
TMP_ROOT="${TMP_ROOT:-D:/projects/Python/ce_ab_tmp}"
LEGACY_DATA="${TMP_ROOT}/legacy"
NEW_DATA="${TMP_ROOT}/new"
rm -rf "$TMP_ROOT" 2>/dev/null || true
mkdir -p "$LEGACY_DATA" "$NEW_DATA"

LEGACY_BIN="${LEGACY_WT}/target/release/context-engine-rs.exe"
NEW_BIN="${NEW_DIR}/target/release/context-engine-rs.exe"

# repo_id = urlsafe-base64(no-pad) of the normalized (lowercased, backslash) path.
norm_repo() { printf '%s' "$1" | tr '/' '\\' | tr 'A-Z' 'a-z'; }
REPO_NORM="$(norm_repo "$REPO")"
REPO_ID="$(printf '%s' "$REPO_NORM" | base64 | tr '+/' '-_' | tr -d '=')"

LEGACY_PID=""
NEW_PID=""
cleanup() {
  local code=$?
  echo "[ab_bench] cleanup (exit=${code}) ..."
  if [ "$code" -ne 0 ]; then
    echo "----- legacy.log (tail) -----"; tail -20 "${TMP_ROOT}/legacy.log" 2>/dev/null || true
    echo "----- new.log (tail) -----";    tail -20 "${TMP_ROOT}/new.log" 2>/dev/null || true
  fi
  [ -n "$LEGACY_PID" ] && kill "$LEGACY_PID" 2>/dev/null || true
  [ -n "$NEW_PID" ] && kill "$NEW_PID" 2>/dev/null || true
  # Give RocksDB a moment to release file locks before removing temp dirs.
  sleep 2
  rm -rf "$TMP_ROOT" 2>/dev/null || true
}
trap cleanup EXIT

wait_up() {  # wait_up <url>
  local url="$1" i
  for i in $(seq 1 60); do
    if curl -fsS "${url}/api/index-status" >/dev/null 2>&1; then return 0; fi
    sleep 1
  done
  echo "[ab_bench] ERROR: server ${url} did not come up" >&2
  return 1
}

# Poll the PER-REPO status endpoint until the repo's rebuild has finished.
# (The array endpoint lists every configured repo in nondeterministic order, so
# grepping the first "state" could read an always-idle sibling repo. The
# per-repo endpoint is unambiguous.) "Done" = state is no longer "indexing" AND
# a fresh last_indexed_at has been stamped (the temp index starts with none).
#
# NOTE: every command substitution here is `|| true`-guarded. Under `set -e`, a
# grep that finds no match (normal during early polling, before the status JSON
# has the field) returns 1 and would otherwise abort the whole script.
wait_indexed() {  # wait_indexed <url> <label>
  local url="$1" label="$2" i body state indexed_at
  for i in $(seq 1 1800); do
    body="$(curl -fsS "${url}/api/repos/${REPO_ID}/status" 2>/dev/null || true)"
    state="$(printf '%s' "$body" | grep -o '"state":"[a-z]*"' | head -1 | sed 's/.*:"//;s/"//' || true)"
    indexed_at="$(printf '%s' "$body" | grep -o '"last_indexed_at":"[^"]*"' | head -1 || true)"
    if [ "$state" = "error" ]; then
      echo "[ab_bench] ERROR: ${label} index errored: ${body}" >&2; return 1
    fi
    if [ "$state" = "idle" ] && [ -n "$indexed_at" ]; then
      echo "[ab_bench] ${label} index finished (state=idle, ${indexed_at})"
      return 0
    fi
    if [ $((i % 10)) -eq 0 ]; then echo "[ab_bench] ${label} indexing... (${i}s, state=${state:-?})"; fi
    sleep 1
  done
  echo "[ab_bench] ERROR: ${label} index did not finish in time" >&2
  return 1
}

echo "[ab_bench] repo=${REPO}  repo_id=${REPO_ID}"
echo "[ab_bench] legacy_bin=${LEGACY_BIN}"
echo "[ab_bench] new_bin=${NEW_BIN}"

# ── Provision the legacy build (git HEAD = pre-cAST chunker) if missing ──────
# Design B needs a binary built from the SUBMODULE's git HEAD. We isolate it in a
# detached-HEAD worktree so the working tree's uncommitted cAST change is never
# disturbed. Skip if a worktree + binary already exist (reuse the build cache).
if [ ! -x "$LEGACY_BIN" ]; then
  echo "[ab_bench] legacy binary missing — provisioning worktree + build ..."
  HEAD_SHA="$(git -C "$NEW_DIR" rev-parse HEAD)"
  if ! git -C "$NEW_DIR" worktree list | grep -qiF "$(printf '%s' "$LEGACY_WT" | tr '/' '\\')"; then
    git -C "$NEW_DIR" worktree add -d "$LEGACY_WT" "$HEAD_SHA"
  fi
  ( cd "$LEGACY_WT" && cargo build --release --bin context-engine-rs )
fi
[ -x "$LEGACY_BIN" ] || { echo "ERROR: legacy binary missing: $LEGACY_BIN" >&2; exit 1; }

# ── Provision the new (working-tree, cAST) build if missing ──────────────────
if [ ! -x "$NEW_BIN" ]; then
  echo "[ab_bench] new binary missing — building working tree ..."
  ( cd "$NEW_DIR" && cargo build --release --bin context-engine-rs )
fi
[ -x "$NEW_BIN" ]    || { echo "ERROR: new binary missing: $NEW_BIN" >&2; exit 1; }

# ── Boot both servers (isolated ports + temp data dirs) ──────────────────────
echo "[ab_bench] booting legacy server on ${LEGACY_PORT} (data=${LEGACY_DATA}) ..."
RUST_LOG="context_engine_rs=warn,warn" "$LEGACY_BIN" \
  --port "$LEGACY_PORT" --bind 127.0.0.1 --data-dir "$LEGACY_DATA" \
  >"${TMP_ROOT}/legacy.log" 2>&1 &
LEGACY_PID=$!

echo "[ab_bench] booting new server on ${NEW_PORT} (data=${NEW_DATA}) ..."
RUST_LOG="context_engine_rs=warn,warn" "$NEW_BIN" \
  --port "$NEW_PORT" --bind 127.0.0.1 --data-dir "$NEW_DATA" \
  >"${TMP_ROOT}/new.log" 2>&1 &
NEW_PID=$!

wait_up "$LEGACY_URL"
wait_up "$NEW_URL"

# ── Trigger fresh full rebuilds (force_rebuild) of the repo on each ──────────
echo "[ab_bench] triggering rebuild on legacy ..."
curl -fsS -X POST "${LEGACY_URL}/api/repos/${REPO_ID}/rebuild" >/dev/null
echo "[ab_bench] triggering rebuild on new ..."
curl -fsS -X POST "${NEW_URL}/api/repos/${REPO_ID}/rebuild" >/dev/null

# Small grace so the trigger flips state to "indexing" before we poll.
sleep 3
wait_indexed "$LEGACY_URL" "legacy"
wait_indexed "$NEW_URL" "new"

# ── Run the identical harness + query path against each, emit A/B artifact ───
echo "[ab_bench] running chunk_bench --ab ..."
CHUNK_BENCH="${NEW_DIR}/target/release/chunk_bench.exe"
if [ ! -x "$CHUNK_BENCH" ]; then
  cd "$NEW_DIR" && cargo build --release --bin chunk_bench
fi
"$CHUNK_BENCH" \
  "$REPO" "$LEGACY_URL" "$OUT_ABS" --ab --new-server "$NEW_URL"

echo "[ab_bench] done -> ${OUT_ABS}"
