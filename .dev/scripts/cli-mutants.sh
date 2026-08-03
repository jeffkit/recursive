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
# Default: parallel copy mode (--jobs = CPU count) — real source never
# mutated, uncommitted changes safe, and total time ~time/cores vs
# ~60s/mutant single-threaded (2026-08-03 gate-timeout incident).
JOBS=$(sysctl -n hw.ncpu 2>/dev/null || nproc 2>/dev/null || echo 4)

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
# bash 3.2 (macOS default) turns "${ARGS[@]:-}" into ONE empty-string arg when
# the array is empty (the :- default kicks in on the empty expansion) → $#=1,
# $1="" → the `[[ $# -gt 0 ]]` branch fires with an empty file → `--file ""`
# (empty glob) → cargo-mutants mutates the WHOLE crate. ${ARGS[@]:0} is the
# safe form across versions (same pattern as tui-mutants.sh).
set -- ${ARGS[@]:0}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

MUTATED_FILES=()

cleanup_mutants() {
  local rc=$?
  # NOTE: `$?` here is captured as the FIRST statement so a syntax-error / unexpected
  # exit is faithfully re-emitted, not masked by the trap's own success.
  # (mutants-gate-false-green bug: sh + <(...)-syntax-error produced exit 0.)
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
  exit "$rc"
}
trap cleanup_mutants EXIT

assert_clean() {
  if [[ "$JOBS" -gt 1 ]]; then
    # copy mode: real source never mutated — uncommitted changes are safe.
    return 0
  fi
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
  # NOTE: observed (2026-08-03, cargo-mutants 27.1.0) that exit 3 can accompany a report
  # containing MISSED lines — blanket-passing rc=3 would false-green them. Check the text.
  local rc=0 out
  out=$(mktemp)
  if [[ -n "$FEATURES" ]]; then
    cargo mutants -p "$CRATE" --features "$FEATURES" "${mode_args[@]}" "$@" 2>&1 | tee "$out" || rc=${PIPESTATUS[0]}
  else
    cargo mutants -p "$CRATE" "${mode_args[@]}" "$@" 2>&1 | tee "$out" || rc=${PIPESTATUS[0]}
  fi
  if [[ "$rc" -eq 3 ]]; then
    if grep -q "MISSED" "$out"; then
      echo "note: cargo-mutants exited 3 but the report contains MISSED mutants — treating as FAILURE" >&2
      rc=2
    else
      echo "note: cargo-mutants exited 3 (timeouts only, no missed mutants) — treating as pass" >&2
      rc=0
    fi
  fi
  rm -f "$out"
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
  # Temp file instead of `< <(...)` (bash-only) — works under sh too.
  # (mutants-gate bug: <(...)-syntax-error under sh silently exited 0.)
  _tmp=$(mktemp)
  find "crates/$CRATE/src" -name '*.rs' > "$_tmp"
  while IFS= read -r f; do MUTATED_FILES+=("$f"); done < "$_tmp"
  rm -f "$_tmp"
  assert_clean "${MUTATED_FILES[@]}"
  run_mutants --no-shuffle
  exit 0
elif [[ "${1:-}" == "--dir" ]]; then
  shift
  DIR="${1:?--dir requires a path}"
  echo "Mutating directory: $DIR" >&2
  _tmp=$(mktemp)
  find "$DIR" -name '*.rs' > "$_tmp"
  while IFS= read -r f; do MUTATED_FILES+=("$f"); done < "$_tmp"
  rm -f "$_tmp"
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
