#!/usr/bin/env bash
# e2e-run.sh — run a single argusai E2E suite in the current worktree,
# encapsulating the setup → run → clean lifecycle so the caller never has
# to remember the two traps that bite manual `argusai` CLI use:
#
#   1. `argusai run` does NOT auto-create the service container — it must be
#      preceded by `argusai setup`. Running `argusai run` alone fails every
#      case with "No such container: recursive-e2e".
#   2. Setting WORKTREE_ID triggers namespace isolation (container renamed
#      to `recursive-e2e-<ns>`), but suite YAMLs reference `container:
#      recursive-e2e` — the CLI does not resolve the namespaced name, so
#      cases 404. (The MCP path used by e2e-gate.sh DOES resolve it; this
#      CLI wrapper deliberately leaves WORKTREE_ID unset for single-worktree
#      dev use. For parallel worktree runs, use e2e-gate.sh / the flowcast
#      flow instead.)
#
# Usage:
#   .dev/scripts/e2e-run.sh <suite-id> [--no-build]
#
# Exit code: 0 iff every case in the suite passed. `argusai run` always
# exits 0 even on case failures, so the pass/fail is parsed from the
# summary line.
#
# Prereqs: Docker + the `argusai` CLI on PATH.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
E2E_DIR="$REPO_ROOT/e2e"

SUITE="${1:-}"
NO_BUILD=0
[[ "${2:-}" == "--no-build" ]] && NO_BUILD=1

if [[ -z "$SUITE" ]]; then
  echo "usage: $0 <suite-id> [--no-build]" >&2
  echo "e.g.   $0 claude-json-stream" >&2
  exit 2
fi

if ! command -v argusai >/dev/null 2>&1; then
  echo "[e2e-run] argusai CLI not found on PATH" >&2
  exit 3
fi

# Never let a namespaced WORKTREE_ID leak in from the caller's env and break
# the `container: recursive-e2e` references in suite YAMLs.
unset WORKTREE_ID

cd "$E2E_DIR"

# Always clean any leftover environment first so a stale container from a
# previous aborted run does not shadow a fresh one.
argusai clean -c e2e.yaml >/dev/null 2>&1 || true

if [[ "$NO_BUILD" -eq 0 ]]; then
  echo "[e2e-run] building image..."
  argusai build -c e2e.yaml 2>&1 | tail -2 || { echo "[e2e-run] build failed" >&2; exit 4; }
fi

echo "[e2e-run] setup..."
argusai setup -c e2e.yaml 2>&1 | tail -2 || { echo "[e2e-run] setup failed" >&2; argusai clean -c e2e.yaml >/dev/null 2>&1 || true; exit 5; }

# Run the suite and capture stdout for summary parsing (`argusai run` exits 0
# even when cases fail).
RUN_OUT="$(argusai run -c e2e.yaml -s "$SUITE" --timeout 180000 2>&1)"
echo "$RUN_OUT" | tail -25

# Always tear down so no container lingers to collide with the next run.
argusai clean -c e2e.yaml >/dev/null 2>&1 || true

# Parse the summary line: "Summary: N passed, M failed, K skipped".
SUMMARY="$(echo "$RUN_OUT" | rg '^Summary:' | tail -1)"
FAILED="$(echo "$SUMMARY" | rg -o '[0-9]+ failed' | head -1 | rg -o '^[0-9]+' || echo 0)"

if [[ "$FAILED" -eq 0 ]]; then
  echo "[e2e-run] $SUITE PASSED ✓"
  exit 0
else
  echo "[e2e-run] $SUITE FAILED ✗ ($FAILED case(s))" >&2
  exit 1
fi
