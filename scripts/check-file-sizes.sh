#!/usr/bin/env bash
# check-file-sizes.sh — enforce line-count thresholds for Rust source files.
#
# Thresholds (configurable via env):
#   MAX_SRC_LINES  — src/ .rs files      (default: 800)
#   MAX_TEST_LINES — tests/ .rs files    (default: 1000)
#
# Output: table of files exceeding thresholds, one row per violation.
# Exit 0 when all files are within limits; exit 1 (soft-fail) when
# any file exceeds its threshold.  self-improve.sh treats this as a
# warning, not a rollback trigger.
#
# Usage:
#   scripts/check-file-sizes.sh              # check all
#   scripts/check-file-sizes.sh --json       # machine-readable output
#   MAX_SRC_LINES=600 scripts/check-file-sizes.sh   # tighter check

set -euo pipefail

MAX_SRC_LINES="${MAX_SRC_LINES:-800}"
MAX_TEST_LINES="${MAX_TEST_LINES:-1000}"
JSON_OUT=0

if [[ "${1:-}" == "--json" ]]; then
  JSON_OUT=1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# ---- Collect file sizes -----------------------------------------------------
declare -a VIOLATIONS
declare -a ALL_FILES

collect() {
  local dir="$1"
  local max_lines="$2"
  local label="$3"

  while IFS= read -r -d '' file; do
    lines=$(wc -l < "$file" | tr -d ' ')
    ALL_FILES+=("$file $lines $label")
    if [[ "$lines" -gt "$max_lines" ]]; then
      VIOLATIONS+=("$file $lines $label $max_lines")
    fi
  done < <(find "$dir" -name '*.rs' -print0 2>/dev/null)
}

collect "src"    "$MAX_SRC_LINES"  "src"
collect "tests"  "$MAX_TEST_LINES" "tests"

# ---- Output ------------------------------------------------------------------
if [[ $JSON_OUT -eq 1 ]]; then
  # JSON output
  {
    echo '{'
    echo '  "thresholds": {'
    echo '    "src": '"$MAX_SRC_LINES"','
    echo '    "tests": '"$MAX_TEST_LINES"''
    echo '  },'
    echo '  "files": ['
    first=1
    for entry in "${ALL_FILES[@]}"; do
      read -r file lines label <<< "$entry"
      exceeded="false"
      [[ "$lines" -gt "${MAX_SRC_LINES}" && "$label" == "src" ]] && exceeded="true"
      [[ "$lines" -gt "${MAX_TEST_LINES}" && "$label" == "tests" ]] && exceeded="true"
      [[ $first -eq 1 ]] && first=0 || echo ','
      printf '    {"file": "%s", "lines": %d, "category": "%s", "exceeded": %s}' \
        "$file" "$lines" "$label" "$exceeded"
    done
    echo ''
    echo '  ],'
    echo '  "violations": '"${#VIOLATIONS[@]}"''
    echo '}'
  }
else
  # Human-readable table
  echo ""
  echo "=== File Size Check ==="
  echo "Thresholds: src/ ≤ ${MAX_SRC_LINES} lines, tests/ ≤ ${MAX_TEST_LINES} lines"
  echo ""

  if [[ ${#VIOLATIONS[@]} -eq 0 ]]; then
    cat <<EOF
✓ All files within limits.

  Top 5 largest files:
EOF
    # Sort by line count descending, take top 5
    for entry in "${ALL_FILES[@]}"; do
      echo "$entry"
    done | sort -k2 -nr | head -5 | while read -r file lines label; do
      printf "  %4d  %s  (%s)\n" "$lines" "$file" "$label"
    done
    echo ""
    exit 0
  fi

  echo "⚠ Files exceeding thresholds:"
  echo ""
  printf "  %6s  %-55s  %s\n" "LINES" "FILE" "THRESHOLD"
  printf "  %6s  %-55s  %s\n" "------" "------------------------------------------------------" "---------"
  for entry in "${VIOLATIONS[@]}"; do
    read -r file lines label max_lines <<< "$entry"
    printf "  %6d  %-55s  %s (%d)\n" "$lines" "$file" "$label" "$max_lines"
  done
  echo ""
  echo "  ${#VIOLATIONS[@]} files exceed thresholds (soft-fail)."
  echo ""
fi

# Exit non-zero when violations exist (soft-fail)
[[ ${#VIOLATIONS[@]} -eq 0 ]] && exit 0 || exit 1
