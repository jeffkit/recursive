#!/usr/bin/env bash
# tui-mutants.sh — scoped mutation testing for recursive-tui (stage 3/5).
#
# The "effectiveness loop": after the AI writes/strengthens tests for a
# TUI change, run this to check whether those tests actually pin down the
# changed behaviour. cargo-mutants mutates the touched source and re-runs
# the test suite; any mutant that SURVIVES (tests still pass) marks a gap
# in coverage — the test for that behaviour is missing or too weak.
#
# Usage:
#   tui-mutants.sh                         # auto-detect files changed vs main
#   tui-mutants.sh <file>...               # mutate specific files
#   tui-mutants.sh --dir src/app/render.rs # mutate a directory (recursive)
#   tui-mutants.sh --all                   # mutate the whole crate (slow)
#
# Exit code is non-zero if any mutant survives, so this can gate a commit.
#
# Prereq: `cargo install cargo-mutants` (global). The CI / self-improve
# environment is expected to have it on PATH.
set -euo pipefail

CRATE="recursive-tui"
# test-utils is a dev-dependency feature of recursive-tui; passing it
# explicitly is harmless and keeps the runner robust if the dev-dep is
# ever removed.
FEATURES="recursive/test-utils"

if ! command -v cargo-mutants >/dev/null 2>&1; then
  echo "error: cargo-mutants not installed. Run: cargo install cargo-mutants" >&2
  exit 2
fi

# Resolve the worktree root (this script lives in <root>/.dev/scripts/).
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

ARGS=()
if [[ "${1:-}" == "--all" ]]; then
  echo "Mutating the whole $CRATE crate (this can take a while)…" >&2
  cargo mutants --in-place -p "$CRATE" --features "$FEATURES" \
    --no-shuffle
  exit 0
elif [[ "${1:-}" == "--dir" ]]; then
  shift
  DIR="${1:?--dir requires a path}"
  echo "Mutating directory: $DIR" >&2
  cargo mutants --in-place -p "$CRATE" --features "$FEATURES" \
    --dir "$DIR" --no-shuffle
  exit 0
elif [[ $# -gt 0 ]]; then
  echo "Mutating files: $*" >&2
  for f in "$@"; do
    ARGS+=(--file "$f")
  done
  cargo mutants --in-place -p "$CRATE" --features "$FEATURES" \
    --no-shuffle "${ARGS[@]}"
  exit 0
fi

# Default: auto-detect files changed on this branch vs main (plus any
# uncommitted edits). This is the "改某文件 → 杀该文件变异点" rule — only
# mutate what the current change touches, keeping the run fast.
MAP_TO_CRATE="s#^crates/$CRATE/##"

CHANGED=$( {
  git diff --name-only main...HEAD 2>/dev/null || true
  git diff --name-only 2>/dev/null || true
} | grep "^crates/$CRATE/src/" | sort -u || true )

if [[ -z "$CHANGED" ]]; then
  echo "No recursive-tui source files changed vs main. Pass file paths or --all." >&2
  exit 0
fi

echo "Auto-detected changed files under $CRATE:" >&2
echo "$CHANGED" | sed 's/^/  /' >&2

FILE_ARGS=()
while IFS= read -r line; do
  FILE_ARGS+=(--file "$line")
done <<< "$CHANGED"

cargo mutants --in-place -p "$CRATE" --features "$FEATURES" \
  --no-shuffle "${FILE_ARGS[@]}"
