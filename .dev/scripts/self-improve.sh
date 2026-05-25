#!/usr/bin/env bash
# self-improve.sh — DEVELOPER-only wrapper: invoke Recursive against its own source.
#
# This script is NOT part of the shipping product. Self-improvement is a
# workflow the developer runs in their workspace; the agent itself doesn't
# know about it.
#
# Safety net:
#   - Requires a clean working tree and a baseline commit before running.
#   - On success (agent exit 0 AND post-run `cargo test` green): auto-commits
#     all changes with a descriptive message.
#   - On any failure: hard-resets to baseline. Nothing in src/ survives.
#
# Usage:
#   .dev/scripts/self-improve.sh .dev/goals/02-foo.md
#   .dev/scripts/self-improve.sh "inline goal text"
#
# Env:
#   RECURSIVE_API_KEY (required; falls back to GLM_API_KEY / MINIMAX_API_KEY)
#   RECURSIVE_API_BASE (default: MiniMax)
#   RECURSIVE_MODEL    (default: MiniMax-M2)
#   RECURSIVE_MAX_STEPS (default: 30)
#   RECURSIVE_NO_COMMIT (set to 1 to skip the auto-commit step)

set -euo pipefail

# Resolve repo root (two levels up: .dev/scripts/ -> .dev/ -> repo root).
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DEV_DIR="$REPO_ROOT/.dev"
cd "$REPO_ROOT"

if [[ $# -lt 1 ]]; then
  echo "usage: $0 <goal-file-or-text>" >&2
  exit 2
fi

# Resolve the goal: file content if exists, else literal text.
if [[ -f "$1" ]]; then
  GOAL_SOURCE="$1"
  GOAL_BODY="$(cat "$1")"
  GOAL_TAG="$(basename "$1" | sed -E 's/^[0-9]+-//; s/\.md$//')"
else
  GOAL_SOURCE="<inline>"
  GOAL_BODY="$1"
  GOAL_TAG="inline"
fi

# ---- Git safety net pre-flight ---------------------------------------------

if ! git rev-parse --verify HEAD >/dev/null 2>&1; then
  echo "error: no baseline commit. Commit the current state first so failures can roll back." >&2
  exit 2
fi
BASELINE_HEAD="$(git rev-parse HEAD)"
BASELINE_SHORT="$(git rev-parse --short HEAD)"

if [[ -n "$(git status --porcelain)" ]]; then
  echo "error: working tree dirty. Commit or stash before running self-improve." >&2
  git status --short >&2
  exit 2
fi

# ---- Build system prompt ----------------------------------------------------

SYSPROMPT_FILE="$(mktemp -t recursive-sysprompt.XXXXXX)"
trap 'rm -f "$SYSPROMPT_FILE"' EXIT

{
  echo "You are Recursive, a Rust coding agent operating on your OWN source code."
  echo "Tools: read_file, write_file, list_dir, run_shell. Sandboxed to workspace."
  echo ""
  echo "=== .dev/AGENTS.md (project contract) ==="
  cat "$DEV_DIR/AGENTS.md"
  echo ""
  if [[ -d "$DEV_DIR/journal" && -n "$(ls -A "$DEV_DIR/journal" 2>/dev/null || true)" ]]; then
    echo "=== Recent journal (last 3 entries, most recent first) ==="
    ls -1t "$DEV_DIR/journal"/*.md 2>/dev/null | head -3 | while read -r f; do
      echo "--- $(basename "$f") ---"
      cat "$f"
      echo ""
    done
  fi
} > "$SYSPROMPT_FILE"

# ---- Env defaults -----------------------------------------------------------

export RECURSIVE_API_BASE="${RECURSIVE_API_BASE:-https://api.minimaxi.com/v1}"
export RECURSIVE_MODEL="${RECURSIVE_MODEL:-MiniMax-M2}"
export RECURSIVE_MAX_STEPS="${RECURSIVE_MAX_STEPS:-30}"
export RECURSIVE_API_KEY="${RECURSIVE_API_KEY:-${MINIMAX_API_KEY:-${GLM_API_KEY:-}}}"
if [[ -z "${RECURSIVE_API_KEY}" ]]; then
  echo "error: set RECURSIVE_API_KEY (or MINIMAX_API_KEY / GLM_API_KEY)" >&2
  exit 2
fi

# Use release build if available, else dev.
if [[ -x ./target/release/recursive ]]; then
  BIN=./target/release/recursive
else
  cargo build -q
  BIN=./target/debug/recursive
fi

TS="$(date -u +%Y%m%dT%H%M%SZ)"
LOG="$DEV_DIR/journal/run-${TS}.md"

{
  echo "# Run ${TS}"
  echo ""
  echo "- goal source: ${GOAL_SOURCE}"
  echo "- goal tag:    ${GOAL_TAG}"
  echo "- model:       ${RECURSIVE_MODEL}"
  echo "- baseline:    ${BASELINE_SHORT}"
  echo ""
  echo "## Goal"
  echo ""
  echo '```'
  echo "${GOAL_BODY}"
  echo '```'
  echo ""
  echo "## Agent transcript"
  echo ""
  echo '```'
} > "$LOG"

# ---- Run the agent ----------------------------------------------------------

set +e
"$BIN" --workspace . \
  --system-prompt-file "$SYSPROMPT_FILE" \
  --log warn \
  run "$GOAL_BODY" 2>&1 | tee -a "$LOG"
AGENT_STATUS=${PIPESTATUS[0]}
set -e

{
  echo '```'
  echo ""
} >> "$LOG"

# ---- Post-run verification + commit/reset ----------------------------------

verdict_and_exit() {
  local verdict="$1"
  local detail="$2"

  {
    echo "## Result"
    echo ""
    echo "- agent exit status: ${AGENT_STATUS}"
    echo "- verdict:           ${verdict}"
    [[ -n "$detail" ]] && echo "- detail:            ${detail}"
    echo "- changed files (before action):"
    echo '```'
    git status --short
    echo '```'
  } >> "$LOG"

  case "$verdict" in
    committed)
      # Commit log + agent output together.
      git add -A
      git commit --quiet -m "self-improve(${GOAL_TAG}): ${detail}

Baseline: ${BASELINE_SHORT}
Model:    ${RECURSIVE_MODEL}
Goal:     ${GOAL_SOURCE}
"
      echo ""
      echo "=== ✓ committed: $(git log --oneline -1) ==="
      echo "=== journaled to ${LOG} ==="
      exit 0
      ;;
    rolled-back)
      git reset --hard "${BASELINE_HEAD}" --quiet
      # Re-create the journal entry post-reset (reset wiped it).
      mkdir -p "$DEV_DIR/journal"
      cat > "$LOG.tmp" <<EOF
# Run ${TS} (ROLLED BACK)

- goal source: ${GOAL_SOURCE}
- model:       ${RECURSIVE_MODEL}
- baseline:    ${BASELINE_SHORT}
- verdict:     rolled-back
- detail:      ${detail}
EOF
      mv "$LOG.tmp" "$LOG"
      echo ""
      echo "=== ✗ rolled back to ${BASELINE_SHORT} (${detail}) ==="
      echo "=== journaled to ${LOG} ==="
      exit 1
      ;;
    skip-commit)
      echo ""
      echo "=== ✓ agent succeeded but RECURSIVE_NO_COMMIT=1, leaving working tree dirty ==="
      echo "=== journaled to ${LOG} ==="
      exit 0
      ;;
  esac
}

if [[ "$AGENT_STATUS" -ne 0 ]]; then
  verdict_and_exit "rolled-back" "agent exited with status ${AGENT_STATUS}"
fi

# Defence in depth: re-run cargo test from outside the agent's transcript.
if ! cargo test --quiet >/dev/null 2>&1; then
  verdict_and_exit "rolled-back" "post-agent cargo test failed"
fi

if [[ -z "$(git status --porcelain)" ]]; then
  verdict_and_exit "skip-commit" "agent succeeded but made no file changes"
fi

if [[ "${RECURSIVE_NO_COMMIT:-0}" == "1" ]]; then
  verdict_and_exit "skip-commit" "RECURSIVE_NO_COMMIT=1 set"
fi

CHANGED_COUNT="$(git status --porcelain | wc -l | tr -d ' ')"
verdict_and_exit "committed" "${CHANGED_COUNT} files changed, cargo test green"
