#!/usr/bin/env bash
# run-e2e.sh — Run all E2E integration tests for Recursive.
#
# Usage:
#   ./e2e/run-e2e.sh              # Run all tests
#   ./e2e/run-e2e.sh 01 03       # Run specific tests by number
#   ./e2e/run-e2e.sh --no-judge  # Skip LLM-as-judge (cheaper)
#
# Environment:
#   DEEPSEEK_API_KEY (required for live tests)
#   RECURSIVE_BIN    (path to binary, default: auto-detect)
#   E2E_JUDGE=0      (disable LLM judge)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# Parse args
FILTER=""
for arg in "$@"; do
  case "$arg" in
    --no-judge) export E2E_JUDGE=0 ;;
    *) FILTER="$FILTER $arg" ;;
  esac
done

# Ensure binary exists
BIN="${RECURSIVE_BIN:-}"
if [[ -z "$BIN" ]]; then
  if [[ -x ./target/release/recursive ]]; then
    BIN=./target/release/recursive
  elif [[ -x ./target/debug/recursive ]]; then
    BIN=./target/debug/recursive
  else
    echo "[e2e] Building recursive..."
    cargo build --release -q
    BIN=./target/release/recursive
  fi
fi
export RECURSIVE_BIN="$BIN"

echo "========================================="
echo " Recursive E2E Integration Tests"
echo " Binary: $BIN"
echo " API: ${RECURSIVE_API_BASE:-https://api.deepseek.com/v1}"
echo " Model: ${RECURSIVE_MODEL:-deepseek-chat}"
echo " Judge: ${E2E_JUDGE:-1}"
echo "========================================="
echo ""

# Collect test scripts
TESTS_DIR="$REPO_ROOT/e2e/tests"
TOTAL=0
PASSED=0
FAILED=0
SKIPPED=0

for test_script in "$TESTS_DIR"/*.sh; do
  test_name="$(basename "$test_script" .sh)"
  test_num="${test_name%%-*}"

  # Filter
  if [[ -n "${FILTER// /}" ]]; then
    MATCH=0
    for f in $FILTER; do
      if [[ "$test_num" == "$f" ]] || [[ "$test_name" == *"$f"* ]]; then
        MATCH=1
        break
      fi
    done
    [[ "$MATCH" -eq 0 ]] && continue
  fi

  TOTAL=$((TOTAL + 1))
  echo "─────────────────────────────────────────"
  echo " [$test_num] $test_name"
  echo "─────────────────────────────────────────"

  set +e
  bash "$test_script"
  EXIT=$?
  set -e

  case "$EXIT" in
    0) PASSED=$((PASSED + 1)); echo " ✓ PASSED" ;;
    *)
      # Check if it was a skip
      if grep -q "SKIP" <<< "$(tail -3 "$test_script" 2>/dev/null)"; then
        SKIPPED=$((SKIPPED + 1))
        echo " ⊘ SKIPPED"
      else
        FAILED=$((FAILED + 1))
        echo " ✗ FAILED (exit $EXIT)"
      fi
      ;;
  esac
  echo ""
done

echo "========================================="
echo " Results: $PASSED passed / $FAILED failed / $SKIPPED skipped (of $TOTAL)"
echo "========================================="

if [[ "$FAILED" -gt 0 ]]; then
  exit 1
fi
exit 0
