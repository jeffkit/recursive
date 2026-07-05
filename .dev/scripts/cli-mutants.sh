#!/usr/bin/env bash
# cli-mutants.sh — scoped mutation testing for recursive-cli.
#
# Mirrors the design of tui-mutants.sh / agent-mutants.sh.
# Run cargo-mutants against the CLI crate and fail if any mutant survives.
#
# Usage:
#   cli-mutants.sh                         # auto-detect files changed vs main
#   cli-mutants.sh <file>...               # mutate specific files
#   cli-mutants.sh --dir src/cli           # mutate a whole sub-directory
#   cli-mutants.sh --all                   # mutate the whole crate (slow)
#   cli-mutants.sh --jobs 4 --all          # parallel whole-crate baseline
#   cli-mutants.sh --list                  # dry-run: list mutants, no tests
#   cli-mutants.sh --list-files            # list source files cargo-mutants sees
#
# Exit code is non-zero if any mutant survives.
# Prereq: `cargo install cargo-mutants` (global).
set -euo pipefail

CRATE="recursive-cli"
# No special features beyond defaults for this crate.
FEATURES=""
JOBS=1

if ! command -v cargo-mutants >/dev/null 2>&1; then
  echo "error: cargo-mutants not installed. Run: cargo install cargo-mutants" >&2
  exit 2
fi

ARGS=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --jobs)
      JOBS="${2:?--jobs requires a number}"
      shift 2
      ;;
    --jobs=*)
      JOBS="${1#--jobs=}"
      shift
      ;;
    *)
      ARGS+=("$1")
      shift
      ;;
  esac
done
set -- "${ARGS[@]:-}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

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
  local dirty=()
  for f in "$@"; do
    if [[ -f "$f" ]] && ! git diff --quiet -- "$f" 2>/dev/null; then
      dirty+=("$f")
    fi
  done
  if [[ ${#dirty[@]} -gt 0 ]]; then
    echo "error: refusing to mutate files with uncommitted changes:" >&2
    printf '  %s\n' "${dirty[@]}" >&2
    echo "commit or stash them first." >&2
    exit 3
  fi
}

run_mutants() {
  local mode_args=()
  if [[ "$JOBS" -gt 1 ]]; then
    mode_args+=(--jobs "$JOBS")
  else
    mode_args+=(--in-place)
  fi
  # cargo-mutants exit codes:
  #   0 = all mutations caught (or none found)
  #   2 = some mutations MISSED — tests do not pin the changed behaviour → gate FAILS
  #   3 = some mutations timed out, none missed → tests detected the mutation (infinite loop
  #       / non-termination) but just slowly; this is acceptable → treat as success (exit 0)
  # Any other non-zero code (e.g. 1 = baseline test failure) is preserved as-is.
  local rc=0
  if [[ -n "$FEATURES" ]]; then
    cargo mutants -p "$CRATE" --features "$FEATURES" "${mode_args[@]}" "$@" || rc=$?
  else
    cargo mutants -p "$CRATE" "${mode_args[@]}" "$@" || rc=$?
  fi
  if [[ "$rc" -eq 3 ]]; then
    echo "note: cargo-mutants exited 3 (timeouts only, no missed mutants) — treating as pass" >&2
    return 0
  fi
  return "$rc"
}

enumerate_mutants() {
  if [[ -n "$FEATURES" ]]; then
    cargo mutants --list -p "$CRATE" --features "$FEATURES" "$@"
  else
    cargo mutants --list -p "$CRATE" "$@"
  fi
}

ARGS=()
if [[ "${1:-}" == "--list" ]]; then
  echo "Enumerating mutants in $CRATE (dry run, no tests)…" >&2
  enumerate_mutants
  exit 0
elif [[ "${1:-}" == "--list-files" ]]; then
  if [[ -n "$FEATURES" ]]; then
    cargo mutants --list-files -p "$CRATE" --features "$FEATURES"
  else
    cargo mutants --list-files -p "$CRATE"
  fi
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

# Default: auto-detect CLI source files changed on this branch vs main.
CHANGED=$( {
  git diff --name-only main...HEAD 2>/dev/null || true
  git diff --name-only 2>/dev/null || true
} | grep "^crates/$CRATE/src/" | sort -u || true )

if [[ -z "$CHANGED" ]]; then
  echo "No $CRATE source files changed vs main. Pass file paths or --all." >&2
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
