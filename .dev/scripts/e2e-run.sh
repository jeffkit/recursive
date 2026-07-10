#!/usr/bin/env bash
# e2e-run.sh — run a single argusai E2E suite via the MCP path
# (mcp2cli → argusai-mcp), mirroring e2e-gate.sh's lifecycle.
#
# Why the MCP path and not the `argusai` CLI:
#   The `argusai-cli` umbrella package is frozen at 0.12.3, which has a
#   regression where `argusai run` does NOT execute setup `exec` commands
#   (verified: a `sleep 8` in setup yields a 3s wall clock). The 0.14.1
#   release — which carries the issue fixes — only ships `argusai-mcp` /
#   `argusai-core` (no `argusai` CLI bin). So the working entry point is the
#   MCP server, invoked here through mcp2cli.
#
# Lifecycle (argusai 0.14.2, 5 steps): init → build → setup → run → clean.
#
# Success = status:passed AND totals.total > 0 AND totals.failed == 0.
# The total>0 guard rejects the false-green where argusai drops all case
# events. 0.14.1 attributed case events by suite `name` (issue #8), so a
# mismatch between e2e.yaml `name` and the suite yaml `name` silently dropped
# every case → status=passed/total=0. 0.14.2 fixes this: events carry a stable
# `suiteId` (from e2e.yaml `id`) and empty aggregation is recorded as failure.
# e2e.yaml names are still kept aligned with their files as a convention, but
# it is no longer load-bearing. The total>0 guard stays as defense-in-depth.
# See journal manual-20260710-argusai-0.14-upgrade + -0142-followup.
#
# Usage:
#   .dev/scripts/e2e-run.sh <suite-id> [--no-build]
#
# Exit code: 0 iff the suite ran at least one case and every case passed.
#
# Prereqs: Docker, mcp2cli on PATH, argusai-mcp installed (npm i -g argusai-mcp).

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
E2E_PROJECT="$REPO_ROOT/e2e"

SUITE="${1:-}"
NO_BUILD=0
[[ "${2:-}" == "--no-build" ]] && NO_BUILD=1

if [[ -z "$SUITE" ]]; then
  echo "usage: $0 <suite-id> [--no-build]" >&2
  echo "e.g.    $0 claude-json-stream" >&2
  exit 2
fi

# ---- resolve mcp2cli -------------------------------------------------------
MCP2CLI=""
for _c in "$HOME/.local/bin/mcp2cli" "/usr/local/bin/mcp2cli" "/opt/homebrew/bin/mcp2cli"; do
  [[ -x "$_c" ]] && { MCP2CLI="$_c"; break; }
done
[[ -n "$MCP2CLI" ]] || { echo "[e2e-run] mcp2cli not found (uv tool install mcp2cli)" >&2; exit 3; }

# ---- resolve argusai-mcp entry (standalone → bundled → npx) ----------------
ARGUSAI_MCP_BIN=""
for _root in "$(npm root -g 2>/dev/null)" \
    "$HOME/.local/share/fnm/node-versions"/*/installation/lib/node_modules; do
  if [[ -f "$_root/argusai-mcp/dist/index.js" ]]; then
    ARGUSAI_MCP_BIN="$_root/argusai-mcp/dist/index.js"; break
  fi
  if [[ -f "$_root/argusai-cli/node_modules/argusai-mcp/dist/index.js" ]]; then
    ARGUSAI_MCP_BIN="$_root/argusai-cli/node_modules/argusai-mcp/dist/index.js"; break
  fi
done
if [[ -n "$ARGUSAI_MCP_BIN" ]]; then
  _MCP_STDIO_CMD="node $ARGUSAI_MCP_BIN"
elif command -v npx >/dev/null 2>&1; then
  _MCP_STDIO_CMD="npx argusai-mcp"
else
  echo "[e2e-run] argusai-mcp not found (npm i -g argusai-mcp)" >&2; exit 3
fi

# Single-worktree dev use: leave WORKTREE_ID unset so containers are named
# `recursive-e2e` / `aimock` (matching the `container:` refs in suite YAMLs).
unset WORKTREE_ID

SESSION="e2e-run-$$"

# _argus: invoke an argusai MCP tool; return non-zero on tool-level failure
# (mcp2cli returns exit 0 even when the JSON carries success:false).
_argus() {
  local out
  out="$("$MCP2CLI" --session "$SESSION" "$@" 2>&1)"
  local rc=$?
  echo "$out"
  if echo "$out" | python3 -c "import sys,json; d=json.load(sys.stdin); sys.exit(0 if d.get('success',True) else 1)" 2>/dev/null; then
    return $rc
  else
    return 1
  fi
}

cleanup() {
  _argus argus-clean --project-path "$E2E_PROJECT" >/dev/null 2>&1 || true
  "$MCP2CLI" --session-stop "$SESSION" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# ---- lifecycle: init → build → setup → run → clean ------------------------
"$MCP2CLI" --mcp-stdio "$_MCP_STDIO_CMD" --session-start "$SESSION" >/dev/null 2>&1

if ! _argus argus-init --project-path "$E2E_PROJECT" >/dev/null 2>&1; then
  echo "[e2e-run] argus-init failed" >&2; exit 5
fi

if [[ "$NO_BUILD" -eq 0 ]]; then
  echo "[e2e-run] build..."
  _argus argus-build --project-path "$E2E_PROJECT" >/dev/null 2>&1 || echo "[e2e-run] build warned (continuing with existing image)" >&2
fi

echo "[e2e-run] setup..."
if ! _argus argus-setup --project-path "$E2E_PROJECT" >/dev/null 2>&1; then
  echo "[e2e-run] setup failed" >&2; exit 5
fi

echo "[e2e-run] run $SUITE..."
RUN_OUT="$(_argus argus-run --project-path "$E2E_PROJECT" --filter "$SUITE" 2>&1)"
echo "$RUN_OUT" | python3 -c '
import sys, json
raw = sys.stdin.read()
i = raw.find("{")
try:
    d = json.loads(raw[i:])
except Exception:
    print("  (no JSON parsed)")
    sys.exit(0)
data = d.get("data", {}) or {}
t = data.get("totals", {}) or {}
status = data.get("status")
print("  status=%s totals=%s" % (status, t))
for s in data.get("suites", []):
    print("  suite %s: passed=%s failed=%s skipped=%s" % (s.get("id"), s.get("passed"), s.get("failed"), s.get("skipped")))
' 2>&1 || true

# Success: ran at least one case (total>0) with zero failures.
if echo "$RUN_OUT" | python3 -c '
import sys, json
raw = sys.stdin.read()
i = raw.find("{")
d = json.loads(raw[i:])
data = d.get("data", {}) or {}
t = data.get("totals", {}) or {}
sys.exit(0 if (data.get("status") == "passed" and t.get("total", 0) > 0 and t.get("failed", 0) == 0) else 1)
' 2>/dev/null; then
  echo "[e2e-run] $SUITE PASSED ✓"
  exit 0
else
  echo "[e2e-run] $SUITE FAILED ✗" >&2
  exit 1
fi
