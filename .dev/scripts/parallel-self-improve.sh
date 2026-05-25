#!/usr/bin/env bash
# parallel-self-improve.sh — fire-and-watch wrapper around self-improve.sh
# that runs a single (provider, goal) pair in an isolated git worktree.
#
# Pattern: launch this script multiple times (in the background) with
# different provider+goal pairs to parallelise self-improvement cycles.
# Each worktree is independent — its own checkout, its own branch, its
# own target/ — so two runs cannot stomp on each other.
#
# Usage:
#   .dev/scripts/parallel-self-improve.sh <provider> <goal-file>
#
# Example:
#   .dev/scripts/parallel-self-improve.sh deepseek .dev/goals/07-foo.md &
#   .dev/scripts/parallel-self-improve.sh minimax  .dev/goals/08-bar.md &
#   wait
#
# What it does:
#   1. Validates inputs and required API key env for the requested provider.
#   2. Creates .worktrees/<goal-tag>-<provider>-<TS>/ as a new git worktree
#      branched from current HEAD on a new branch
#      self-improve/<goal-tag>-<provider>-<TS>.
#   3. cd into the worktree and invokes self-improve.sh with
#      RECURSIVE_PROVIDER=<provider>, redirecting stdout+stderr to
#      .dev/runs/<TS>-<tag>-<provider>.log (relative to repo root).
#   4. Prints PID + log path + worktree path so the caller can monitor.
#
# When the inner self-improve.sh finishes:
#   - committed:   1-3 commits land on the worktree's branch
#                  (product commit + observation commit, optionally a
#                   rolled-back journal commit on failure). main is
#                  untouched. Merge back manually after review.
#   - rolled back: worktree is left clean at the baseline of this branch.
#
# Cleanup is intentionally manual — the worktree stays so you can inspect
# the journal and decide whether to keep, cherry-pick, or discard the
# branch. See `.dev/scripts/merge-worktree.sh` (if it exists) or use
# `git worktree remove <path>` + `git branch -D <branch>` to clean up.

set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <provider> <goal-file>" >&2
  echo "       provider: minimax | deepseek | deepseek-pro | glm" >&2
  exit 2
fi

PROVIDER="$1"
GOAL_FILE="$2"

# Resolve repo root and validate inputs from there.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

if [[ ! -f "$GOAL_FILE" ]]; then
  echo "error: goal file not found: $GOAL_FILE" >&2
  exit 2
fi

# Provider key check up-front so we don't create a worktree only to fail.
case "$PROVIDER" in
  minimax)  KEY_NAME="MINIMAX_API_KEY"  ;;
  deepseek|deepseek-flash) KEY_NAME="DEEPSEEK_API_KEY" ;;
  deepseek-pro) KEY_NAME="DEEPSEEK_API_KEY" ;;
  glm)      KEY_NAME="GLM_API_KEY"      ;;
  *)
    echo "error: unknown provider '$PROVIDER' (known: minimax | deepseek | deepseek-pro | glm)" >&2
    exit 2
    ;;
esac

if [[ -z "${!KEY_NAME:-}" ]]; then
  echo "error: $KEY_NAME is not set in the environment" >&2
  exit 2
fi

# Refuse to run on a dirty tree — self-improve.sh checks too, but better to
# fail before creating a worktree we'd have to clean up.
if [[ -n "$(git status --porcelain)" ]]; then
  echo "error: repo root working tree dirty. Commit or stash before launching parallel runs." >&2
  git status --short >&2
  exit 2
fi

GOAL_TAG="$(basename "$GOAL_FILE" | sed -E 's/^[0-9]+-//; s/\.md$//')"
TS="$(date -u +%Y%m%dT%H%M%SZ)-$$"
SHORT_ID="${GOAL_TAG}-${PROVIDER}-${TS}"

WORKTREE_DIR="$REPO_ROOT/.worktrees/$SHORT_ID"
BRANCH="self-improve/$SHORT_ID"
LOG_DIR="$REPO_ROOT/.dev/runs"
LOG_FILE="$LOG_DIR/$SHORT_ID.log"

mkdir -p "$LOG_DIR"

# Create the worktree on a fresh branch from current HEAD.
git worktree add -b "$BRANCH" "$WORKTREE_DIR" HEAD >/dev/null
echo "[parallel] worktree:  $WORKTREE_DIR" >&2
echo "[parallel] branch:    $BRANCH" >&2
echo "[parallel] log:       $LOG_FILE" >&2

# Launch the inner self-improve.sh inside the worktree. Detach with nohup so
# the parent shell can return immediately.
(
  cd "$WORKTREE_DIR"
  # Pass through provider-specific key envs; the inner script will read
  # whichever one matches RECURSIVE_PROVIDER.
  RECURSIVE_PROVIDER="$PROVIDER" \
    nohup ./.dev/scripts/self-improve.sh "$GOAL_FILE" \
    >> "$LOG_FILE" 2>&1 &
  INNER_PID=$!
  echo "[parallel] pid:       $INNER_PID" >&2
  # Detach: write a marker so callers can later find the PID by ID.
  echo "$INNER_PID" > "$LOG_DIR/$SHORT_ID.pid"
)

echo "$SHORT_ID"
