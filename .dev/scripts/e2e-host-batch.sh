#!/usr/bin/env bash
# e2e-host-batch.sh — run all (or a subset of) e2e suites in host mode.
#
# Wraps e2e-run-host.sh, iterating suite ids from e2e/e2e.yaml. Captures each
# suite's pass/fail/skip into a results table and prints a summary at the end.
# Per-suite stdout/stderr is saved to /tmp/e2e-host-batch/<id>.log for triage.
#
# Usage:
#   e2e-host-batch.sh                 # all suites
#   e2e-host-batch.sh smoke basic     # just these ids
#   SKIP_LIVE=1 e2e-host-batch.sh     # skip suites needing DEEPSEEK_API_KEY
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

# Resolve suite ids (from args, or all from master e2e.yaml)
if [ $# -gt 0 ]; then
  IDS=("$@")
else
  IDS=()
  while IFS= read -r line; do
    IDS+=("$line")
  done < <(python3 -c "
import yaml
d = yaml.safe_load(open('e2e/e2e.yaml'))
for s in d['tests']['suites']:
    print(s['id'])
")
fi

# Suites that need a real API key / live network — skip by default unless
# SKIP_LIVE unset. These are integration suites, not replay mock tests.
LIVE_IDS="live deferred-tool-loading"

OUT_DIR="/tmp/e2e-host-batch"
rm -rf "$OUT_DIR"; mkdir -p "$OUT_DIR"

declare -a PASS FAIL SKIP
for id in "${IDS[@]}"; do
  if [ "${SKIP_LIVE:-1}" = "1" ] && [[ " $LIVE_IDS " == *" $id "* ]]; then
    SKIP+=("$id")
    printf '  SKIP  %-28s (live)\n' "$id"
    continue
  fi
  printf '  RUN   %-28s ...' "$id"
  log="$OUT_DIR/$id.log"
  if bash .dev/scripts/e2e-run-host.sh "$id" >"$log" 2>&1; then
    PASS+=("$id"); printf ' PASS\n'
  else
    # Distinguish skipped (total==0) from failed
    if grep -q '"total": 0' "$log" && grep -q '"failed": 0' "$log" 2>/dev/null; then
      SKIP+=("$id"); printf ' SKIP\n'
    else
      FAIL+=("$id"); printf ' FAIL\n'
    fi
  fi
done

echo ""
echo "════════════════════════════════════════════════════"
echo "  PASS: ${#PASS[@]}   FAIL: ${#FAIL[@]}   SKIP: ${#SKIP[@]}   (total ran: ${#IDS[@]})"
echo "════════════════════════════════════════════════════"
[ ${#PASS[@]} -gt 0 ] && { echo "passed:"; printf '  %s\n' "${PASS[@]}"; }
[ ${#FAIL[@]} -gt 0 ] && { echo "FAILED:"; printf '  %s\n' "${FAIL[@]}"; }
[ ${#SKIP[@]} -gt 0 ] && { echo "skipped:"; printf '  %s\n' "${SKIP[@]}"; }
echo ""
echo "logs: $OUT_DIR/<id>.log"

[ ${#FAIL[@]} -eq 0 ]
