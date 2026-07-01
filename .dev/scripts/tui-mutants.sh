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
#   tui-mutants.sh --list                  # dry run: list possible mutants, no tests
#   tui-mutants.sh --list-files            # list source files cargo-mutants sees
#
# Exit code is non-zero if any mutant survives, so this can gate a commit.
# The script guards against --in-place contamination: it refuses to run on
# files with uncommitted changes, and on any exit (incl. SIGINT) restores
# any file still carrying a `cargo-mutants` marker via `git checkout`.
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

# ── in-place contamination guard ──────────────────────────────────────
# cargo-mutants --in-place mutates the real source and restores on a
# *clean* exit. If the run is interrupted (SIGINT, laptop sleep, OOM),
# it can leave `/* ~ changed by cargo-mutants ~ */` markers in the
# source — and a later `git add -A` will silently commit the mutant
# (this actually happened: status.rs had `* 100.0` → `/ 100.0`).
#
# Mitigation: (1) refuse to run if the to-be-mutated files have
# uncommitted changes (so `git checkout --` restore is safe and lossless);
# (2) on ANY exit, scan the mutated files for the marker and `git
# checkout` any that still carry it.
MUTATED_FILES=()

cleanup_mutants() {
  local rc=$?
  if [[ ${#MUTATED_FILES[@]} -gt 0 ]]; then
    local dirty=()
    for f in "${MUTATED_FILES[@]}"; do
      if [[ -f "$f" ]] && grep -q "changed by cargo-mutants" "$f" 2>/dev/null; then
        dirty+=("$f")
      fi
    done
    if [[ ${#dirty[@]} -gt 0 ]]; then
      echo "warn: cargo-mutants left mutations in ${#dirty[@]} file(s); restoring via git checkout:" >&2
      printf '  %s\n' "${dirty[@]}" >&2
      git checkout -- "${dirty[@]}" 2>/dev/null || true
    fi
  fi
  exit $rc
}
trap cleanup_mutants EXIT

assert_clean() {
  # Refuse to mutate files with uncommitted changes — restore would clobber them.
  local dirty=()
  for f in "$@"; do
    if [[ -f "$f" ]] && ! git diff --quiet -- "$f" 2>/dev/null; then
      dirty+=("$f")
    fi
  done
  if [[ ${#dirty[@]} -gt 0 ]]; then
    echo "error: refusing to mutate files with uncommitted changes (restore would clobber):" >&2
    printf '  %s\n' "${dirty[@]}" >&2
    echo "commit or stash them first." >&2
    exit 3
  fi
}

run_mutants() {
  # $@ = extra args (e.g. --no-shuffle, --file ...)
  cargo mutants --in-place -p "$CRATE" --features "$FEATURES" "$@"
}

enumerate_mutants() {
  # Dry run: list possible mutants without mutating source or running tests.
  cargo mutants --list -p "$CRATE" --features "$FEATURES" "$@"
}

ARGS=()
if [[ "${1:-}" == "--list" ]]; then
  # Dry enumerate: list possible mutants across the whole crate, no tests.
  echo "Enumerating mutants in $CRATE (dry run, no tests)…" >&2
  enumerate_mutants
  exit 0
elif [[ "${1:-}" == "--list-files" ]]; then
  cargo mutants --list-files -p "$CRATE" --features "$FEATURES"
  exit 0
elif [[ "${1:-}" == "--all" ]]; then
  echo "Mutating the whole $CRATE crate (this can take a while)…" >&2
  while IFS= read -r f; do MUTATED_FILES+=("$f"); done < <(find "crates/$CRATE/src" -name '*.rs')
  assert_clean "${MUTATED_FILES[@]}"
  run_mutants --no-shuffle
  exit 0
elif [[ "${1:-}" == "--dir" ]]; then
  shift
  DIR="${1:?--dir requires a path}"
  echo "Mutating directory: $DIR" >&2
  while IFS= read -r f; do MUTATED_FILES+=("$f"); done < <(find "$DIR" -name '*.rs')
  assert_clean "${MUTATED_FILES[@]}"
  run_mutants --no-shuffle --dir "$DIR"
  exit 0
elif [[ $# -gt 0 ]]; then
  echo "Mutating files: $*" >&2
  for f in "$@"; do
    ARGS+=(--file "$f")
    MUTATED_FILES+=("$f")
  done
  assert_clean "${MUTATED_FILES[@]}"
  run_mutants --no-shuffle "${ARGS[@]}"
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
  MUTATED_FILES+=("$line")
done <<< "$CHANGED"

assert_clean "${MUTATED_FILES[@]}"

run_mutants --no-shuffle "${FILE_ARGS[@]}"
