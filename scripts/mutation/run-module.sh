#!/usr/bin/env bash
# Run cargo-mutants on a single module file.
#
# Usage:
#   ./scripts/mutation/run-module.sh src/config.rs
#   ./scripts/mutation/run-module.sh src/store/ops.rs
#   ./scripts/mutation/run-module.sh --check src/defender.rs   # compile-only
#
# This script stashes uncommitted src/ changes (cargo-mutants --in-place uses
# git checkout to rollback mutations), runs mutation testing, then restores.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

# Source cargo env if needed
if ! command -v cargo &>/dev/null; then
  source "$HOME/.cargo/env"
fi

CHECK_MODE=""
if [[ "${1:-}" == "--check" ]]; then
  CHECK_MODE="--check"
  shift
fi

FILE="${1:?Usage: $0 [--check] <src/file.rs>}"

if [[ ! -f "$FILE" ]]; then
  echo "Error: file not found: $FILE" >&2
  exit 1
fi

# Map source file path → Rust module path for scoped test execution.
# e.g. src/store/ops.rs       → store::ops
#      src/embedding/cache.rs  → embedding::cache
#      src/query/engine.rs     → query::engine
# This lets cargo-mutants run ONLY the tests for the module being mutated,
# cutting test time from ~68s (all 530 tests) to ~2-5s (module-only tests).
MODULE_PATH=$(echo "$FILE" | sed 's|^src/||' | sed 's|\.rs$||' | tr '/' '::')

# Stash uncommitted src/ changes (--in-place uses git checkout to rollback)
STASHED=""
if ! git diff --quiet -- src/ 2>/dev/null; then
  echo "Stashing uncommitted src/ changes..."
  git stash push -m "cargo-mutants: $(basename "$FILE")" -- src/ 2>/dev/null
  STASHED=1
fi

# Run cargo-mutants with module-scoped tests
echo "Running cargo-mutants on $FILE (tests scoped to $MODULE_PATH)..."
cargo mutants --in-place \
  -f "$FILE" \
  --baseline skip \
  --cargo-test-arg "$MODULE_PATH" \
  $CHECK_MODE

RESULT=$?

# Restore stashed changes
if [[ -n "$STASHED" ]]; then
  echo "Restoring stashed changes..."
  git stash pop 2>/dev/null || {
    echo "WARNING: git stash pop failed. Run 'git stash pop' manually." >&2
  }
fi

exit $RESULT
