#!/usr/bin/env bash
# runs-gc.sh — sweep stale .pid and .log files from .dev/runs/.
#
# A `.pid` file is considered stale when:
#   - the PID it contains does not exist as a process, OR
#   - the PID is alive but is not a recursive/self-improve.sh process
#     (i.e. the OS has reused the slot for an unrelated daemon).
#
# Stale .pid files are deleted alongside their `.notified` siblings (the
# notification marker file). Their `.log` files are kept by default
# because journals + observations already capture the run's results;
# the .log is only useful for live debugging.
#
# Use --logs to also delete .log files. Default behaviour is "preserve
# logs" so an operator can grep them after the fact.
#
# Usage:
#   .dev/scripts/runs-gc.sh [--logs] [--dry-run]
#
# Exit status: 0 on success.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RUNS_DIR="$REPO_ROOT/.dev/runs"

DELETE_LOGS=0
DRY_RUN=0
for arg in "$@"; do
  case "$arg" in
    --logs) DELETE_LOGS=1 ;;
    --dry-run) DRY_RUN=1 ;;
    -h|--help)
      sed -n '2,/^$/p' "${BASH_SOURCE[0]}" | sed 's/^# \?//'
      exit 0
      ;;
    *)
      echo "error: unknown arg: $arg" >&2
      exit 2
      ;;
  esac
done

[[ -d "$RUNS_DIR" ]] || { echo "no $RUNS_DIR — nothing to do"; exit 0; }

stale=0
live=0
total=0

for pidfile in "$RUNS_DIR"/*.pid; do
  [[ -e "$pidfile" ]] || break
  total=$((total + 1))
  pid="$(cat "$pidfile" 2>/dev/null || echo)"

  is_stale=1
  if [[ -n "$pid" ]] && ps -p "$pid" >/dev/null 2>&1; then
    # PID is alive — check whether the command line looks like one of ours.
    cmd="$(ps -p "$pid" -o command= 2>/dev/null || echo)"
    case "$cmd" in
      *self-improve.sh*|*recursive*)
        is_stale=0
        ;;
      *)
        # Live but unrelated process — pid was reused by the OS.
        is_stale=1
        ;;
    esac
  fi

  if [[ "$is_stale" -eq 0 ]]; then
    live=$((live + 1))
    continue
  fi

  base="${pidfile%.pid}"
  victims=("$pidfile")
  [[ -e "${base}.notified" ]] && victims+=("${base}.notified")
  [[ "$DELETE_LOGS" -eq 1 && -e "${base}.log" ]] && victims+=("${base}.log")

  if [[ "$DRY_RUN" -eq 1 ]]; then
    printf "would-delete  %s\n" "${victims[@]}"
  else
    rm -f "${victims[@]}"
  fi
  stale=$((stale + 1))
done

if [[ "$DRY_RUN" -eq 1 ]]; then
  echo "[dry-run] $total .pid file(s) total, $stale stale, $live live"
else
  echo "$total .pid file(s) total, $stale removed, $live preserved"
fi
