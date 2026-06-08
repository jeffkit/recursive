#!/usr/bin/env bash
# land-self-improve.sh — pre-flight checklist for landing a finished
# self-improve run. Encodes the manual recovery workflow used on
# goals 258 and 261 (rebase + workaround-drop + quality-gates +
# merge). Reference: feedback_self_improve_rebase_recovery.md.
#
# Usage:
#   .dev/scripts/land-self-improve.sh <worktree-or-short-id>
#   .dev/scripts/land-self-improve.sh unified-agent-tool-minimax-20260607T082644Z-16189
#   .dev/scripts/land-self-improve.sh .worktrees/unified-agent-tool-minimax-20260607T082644Z-16189
#
# Optional env:
#   SKIP_REBASE=1     skip the rebase step (use only when main has not
#                     advanced since the run launched)
#   SKIP_GATES=1      skip quality gates (DANGEROUS — only for review
#                     branches you intend to fix up later)
#   NO_MERGE=1        stop after the rebase + gates; print the merge
#                     command for the operator to run by hand
#   GOAL_TAG=<tag>    override the goal tag (default: derived from
#                     the worktree path)
#
# What it does (each step asks before continuing if anything looks
# off — the script is conservative and does not auto-merge):
#   1. Locate the worktree by short-id or path.
#   2. Verify the run did not roll back (log does not end with
#      "rolled back to <sha>").
#   3. Check whether main has advanced past the worktree's
#      baseline. If yes, prompt for rebase.
#   4. After rebase, drop the `scripts/apply_goal_*.py` workaround
#      file (minimax workarounds for missing Edit tool) if
#      present, and add `scripts/apply_goal_*.py` to .gitignore
#      if not already.
#   5. Run quality gates:
#        cargo test --lib
#        cargo test --bin recursive
#        cargo clippy --all-targets --all-features -- -D warnings
#        cargo fmt --all -- --check
#   6. If everything green, prompt to merge to main with --no-ff
#      and print a journal template.
#
# This script is idempotent up to step 4: re-running after a
# partial completion is safe.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

if [[ $# -lt 1 ]]; then
  echo "usage: $0 <worktree-or-short-id>" >&2
  echo "" >&2
  echo "examples:" >&2
  echo "  $0 unified-agent-tool-minimax-20260607T082644Z-16189" >&2
  echo "  $0 .worktrees/unified-agent-tool-minimax-20260607T082644Z-16189" >&2
  exit 2
fi

INPUT="$1"

# ---- Step 1: locate worktree -------------------------------------------------

if [[ -d "$INPUT" ]]; then
  WORKTREE_DIR="$(cd "$INPUT" && pwd)"
  SHORT_ID="$(basename "$WORKTREE_DIR")"
elif [[ -d ".worktrees/$INPUT" ]]; then
  WORKTREE_DIR="$REPO_ROOT/.worktrees/$INPUT"
  SHORT_ID="$INPUT"
else
  echo "error: cannot find worktree '$INPUT'" >&2
  echo "  tried: $INPUT (literal)" >&2
  echo "  tried: .worktrees/$INPUT" >&2
  exit 2
fi

LOG_FILE="$REPO_ROOT/.dev/runs/$SHORT_ID.log"
if [[ ! -f "$LOG_FILE" ]]; then
  echo "error: log file not found: $LOG_FILE" >&2
  exit 2
fi

BRANCH="$(cd "$WORKTREE_DIR" && git branch --show-current)"
echo "=== land-self-improve ==="
echo "  worktree: $WORKTREE_DIR"
echo "  branch:   $BRANCH"
echo "  log:      $LOG_FILE"
echo ""

# ---- Step 2: verify the run did not roll back --------------------------------

if tail -5 "$LOG_FILE" | grep -q "rolled back to"; then
  echo "!! run rolled back; nothing to land" >&2
  echo "   inspect: $LOG_FILE" >&2
  echo "   recent journal: ls -t .dev/journal/ | head -3" >&2
  exit 3
fi

if tail -5 "$LOG_FILE" | grep -q "PANIC preserved"; then
  echo "!! run panicked; worktree left dirty for diagnosis" >&2
  echo "   inspect: cd $WORKTREE_DIR && git diff" >&2
  echo "   recover: git reset --hard <baseline-sha-from-journal>" >&2
  exit 3
fi

if ! tail -5 "$LOG_FILE" | grep -q "committed.*self-improve\|journaled to\|agent succeeded"; then
  echo "?? cannot determine run verdict from log tail; showing the last 10 lines:" >&2
  tail -10 "$LOG_FILE" >&2
  echo "" >&2
  read -r -p "Continue anyway? [y/N] " ans
  [[ "$ans" =~ ^[Yy]$ ]] || exit 4
fi

# Find the worktree's baseline (the commit it was branched from).
BASELINE_SHORT="$(cd "$WORKTREE_DIR" && git merge-base --short HEAD origin/main 2>/dev/null \
  || (cd "$WORKTREE_DIR" && git log --oneline --grep="^dev: refresh GitNexus\|^dev: goals" -n 1 --pretty=%h))"
echo "  baseline: $BASELINE_SHORT"

# ---- Step 3: check if main has advanced --------------------------------------

MAIN_HEAD_SHORT="$(git rev-parse --short main)"
MAIN_HEAD="$(git rev-parse main)"
WORKTREE_HEAD_SHORT="$(cd "$WORKTREE_DIR" && git rev-parse --short HEAD)"

echo "  main:     $MAIN_HEAD_SHORT"
echo "  branch:   $WORKTREE_HEAD_SHORT"
echo ""

if [[ "$MAIN_HEAD" != "$(cd "$WORKTREE_DIR" && git merge-base HEAD main)" ]]; then
  if [[ "${SKIP_REBASE:-}" == "1" ]]; then
    echo "!! main has advanced; SKIP_REBASE=1 so proceeding without rebase" >&2
    echo "   the merge WILL delete the user's files if you don't rebase first" >&2
  else
    echo ">> main has advanced past the worktree's baseline."
    echo "   this means the user landed a PR during the run."
    echo "   the worktree's diff vs main will show the user's files as DELETED."
    echo "   recovery pattern (verified on goals 258, 261):"
    echo ""
    echo "   1. cd $WORKTREE_DIR"
    echo "   2. git rebase main        # 1 conflict in src/tools/mod.rs likely"
    echo "   3. resolve by combining imports (keep both HEAD's and rebased)"
    echo "   4. git rebase --continue"
    echo "   5. (continue to step 4 below)"
    echo ""
    read -r -p "Run the rebase now? [Y/n] " ans
    ans="${ans:-Y}"
    if [[ "$ans" =~ ^[Yy]$ ]]; then
      (cd "$WORKTREE_DIR" && git rebase main) || {
        echo "!! rebase had conflicts; resolve them manually then re-run this script" >&2
        exit 5
      }
      echo "   rebase complete; the branch is now linear on top of main"
    else
      echo "!! refusing to land without rebase; the user's files would be deleted" >&2
      exit 6
    fi
  fi
else
  echo "  branch is on top of main; no rebase needed"
fi

# ---- Step 4: drop the minimax workaround script -----------------------------

cd "$WORKTREE_DIR"
WORKAROUND_FILES=$(find scripts -maxdepth 1 -name 'apply_goal_*.py' 2>/dev/null || true)
if [[ -n "$WORKAROUND_FILES" ]]; then
  echo ""
  echo ">> found minimax workaround scripts (workaround for missing Edit tool):"
  echo "$WORKAROUND_FILES" | sed 's/^/   /'
  echo "   per goal-258 convention, these are build artifacts and should not be merged."
  if grep -q '^scripts/apply_goal_\*\.py' .gitignore 2>/dev/null; then
    echo "   .gitignore already has the entry; just removing the files"
    rm -f $WORKAROUND_FILES
  else
    read -r -p "   Remove them and add scripts/apply_goal_*.py to .gitignore? [Y/n] " ans
    ans="${ans:-Y}"
    if [[ "$ans" =~ ^[Yy]$ ]]; then
      rm -f $WORKAROUND_FILES
      echo 'scripts/apply_goal_*.py' >> .gitignore
    else
      echo "!! workaround files left in tree; merge will include them" >&2
    fi
  fi
fi

# ---- Step 5: quality gates ---------------------------------------------------

if [[ "${SKIP_GATES:-}" == "1" ]]; then
  echo "!! SKIP_GATES=1; skipping cargo test/clippy/fmt" >&2
else
  echo ""
  echo ">> running quality gates..."
  set +e
  cargo test --lib 2>&1 | tail -3
  TEST_LIB_RC=$?
  cargo test --bin recursive 2>&1 | tail -3
  TEST_BIN_RC=$?
  cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -3
  CLIPPY_RC=$?
  cargo fmt --all -- --check 2>&1 | tail -3
  FMT_RC=$?
  set -e
  if [[ $TEST_LIB_RC -ne 0 || $TEST_BIN_RC -ne 0 || $CLIPPY_RC -ne 0 || $FMT_RC -ne 0 ]]; then
    echo "" >&2
    echo "!! quality gates failed; fix before merging" >&2
    echo "   test --lib:    exit $TEST_LIB_RC" >&2
    echo "   test --bin:    exit $TEST_BIN_RC" >&2
    echo "   clippy:        exit $CLIPPY_RC" >&2
    echo "   fmt --check:   exit $FMT_RC" >&2
    exit 7
  fi
  echo "   all gates green"
fi

# ---- Step 6: merge to main --------------------------------------------------

cd "$REPO_ROOT"
echo ""
echo ">> ready to merge $BRANCH into main"
git log --oneline -3 "$BRANCH" 2>&1 | sed 's/^/   /'
echo ""

if [[ "${NO_MERGE:-}" == "1" ]]; then
  echo "NO_MERGE=1; not merging. To merge by hand:"
  echo "   git merge --no-ff $BRANCH -m \"Merge $BRANCH\""
  exit 0
fi

read -r -p "Merge to main with --no-ff? [Y/n] " ans
ans="${ans:-Y}"
if [[ ! "$ans" =~ ^[Yy]$ ]]; then
  echo "aborted; not merged. To merge by hand:"
  echo "   git merge --no-ff $BRANCH -m \"Merge $BRANCH\""
  exit 0
fi

git merge --no-ff "$BRANCH" -m "Merge $BRANCH" 2>&1 | tail -5
echo ""
echo ">> merged. Next steps:"
echo "   1. write the journal entry:"
echo "        .dev/journal/manual-\$(date -u +%Y%m%d)-<tag>-rebase.md"
echo "      (use the template below if you want)"
echo "   2. clean up the worktree:"
echo "        git worktree remove .worktrees/$SHORT_ID --force"
echo "        git branch -d $BRANCH"
echo ""
echo "   journal template:"
cat <<'JOURNAL'
# Manual edit: <short-tag>

**Date**: YYYY-MM-DD
**Goal**: <what changed and why — 1 sentence>
**Files touched**: <list from `git diff --stat`>
**Tests added**: <list or "none">
**Recovery actions**: <rebase? drop workaround? which conflicts?>
**Notes**: <anything non-obvious about the agent's output>
JOURNAL
