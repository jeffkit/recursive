#!/usr/bin/env bash
# cli-test-presence.sh — fast "did you write tests?" gate for recursive-cli.
#
# Mirrors tui-test-presence.sh for `crates/recursive-cli/`.
#
# "Test-bearing" means ANY of:
#   - a new/changed file under `crates/recursive-cli/tests/`
#   - a changed `crates/recursive-cli/src/**` file whose diff adds `#[test]`,
#     `#[cfg(test)]`, or a `mod tests` block
#
# Exit codes:
#   0  — no CLI src changed (skip), OR a test-bearing change was detected
#   1  — CLI src changed but no test-bearing change detected
#
# Opt out: `RECURSIVE_CLI_TEST_PRESENCE=0` (document reason in journal).
set -euo pipefail

CRATE="recursive-cli"

if [[ "${RECURSIVE_CLI_TEST_PRESENCE:-1}" == "0" ]]; then
  echo "[cli-test-presence] skipped (RECURSIVE_CLI_TEST_PRESENCE=0)" >&2
  exit 0
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

SRC_CHANGED="$( {
  git diff --name-only main...HEAD 2>/dev/null || true
  git diff --name-only 2>/dev/null || true
} | grep "^crates/$CRATE/src/" | sort -u || true )"

if [[ -z "$SRC_CHANGED" ]]; then
  echo "[cli-test-presence] no $CRATE/src/ files changed — skip" >&2
  exit 0
fi

echo "[cli-test-presence] $CRATE/src/ changed:" >&2
echo "$SRC_CHANGED" | sed 's/^/  /' >&2

has_test_change=0

if {
  git diff --name-only main...HEAD 2>/dev/null || true
  git diff --name-only 2>/dev/null || true
} | grep -qE "^crates/$CRATE/tests/"; then
  echo "[cli-test-presence] found integration-test change under crates/$CRATE/tests/" >&2
  has_test_change=1
fi

while IFS= read -r f; do
  [[ -n "$f" ]] || continue
  added="$( {
    git diff main...HEAD -- "$f" 2>/dev/null || true
    git diff -- "$f" 2>/dev/null || true
  } | grep -E '^\+' | grep -vE '^\+\+\+' || true )"
  if echo "$added" | grep -qE '#\[test\]|#\[cfg\(test\)\]|mod tests'; then
    echo "[cli-test-presence] found new test marker in $f" >&2
    has_test_change=1
  fi
done <<< "$SRC_CHANGED"

if [[ "$has_test_change" -eq 1 ]]; then
  echo "[cli-test-presence] PASS — test-bearing change detected" >&2
  exit 0
fi

cat >&2 <<EOF
[cli-test-presence] FAIL — $CRATE/src/ changed but no test-bearing change detected.
  Add tests covering the new/changed behaviour in the SAME change.
  If this change genuinely needs no new tests (pure refactor), set
  RECURSIVE_CLI_TEST_PRESENCE=0 and document the reason in the journal entry.
EOF
exit 1
