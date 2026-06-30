#!/usr/bin/env bash
# tui-test-presence.sh — fast "did you write tests?" gate for recursive-tui.
#
# The mutation gate (tui-mutants.sh) proves tests *bite*, but it's reactive
# and slow: it only fires after a run, and it costs a resume-fix cycle when
# the agent wrote TUI code with zero or tautological tests. This script is
# the cheap *presence* check that runs first: if a change touches
# `crates/recursive-tui/src/` but adds NO test-bearing code anywhere in the
# TUI surface, fail fast with "add tests" — before the expensive mutation
# gate and before the agent declares done.
#
# "Test-bearing" means ANY of:
#   - a new/changed file under `crates/recursive-tui/tests/` (integration tests)
#   - a changed `crates/recursive-tui/src/**` file whose diff adds `#[test]`,
#     `#[cfg(test)]`, or a `mod tests` block (in-process harness tests)
#   - a change under `crates/tui-pty-harness/` (the PTY harness / its tests)
#
# Exit codes:
#   0  — no TUI src changed (skip), OR a test-bearing change was detected (pass)
#   1  — TUI src changed but no test-bearing change detected (FAIL: add tests)
#
# Opt out: `RECURSIVE_TUI_TEST_PRESENCE=0` for a change that genuinely needs
# no new tests (e.g. a pure formatting/refactor with no behaviour change).
# Document the reason in the journal entry — the mutation gate still runs.
#
# Used as a flow gate (`.flowcast/gates.json` `tui-presence`, onFail:
# resume-fix) ordered BEFORE `tui-mutants`, and as a direct-edit check
# (CLAUDE.md mandatory gates).
set -euo pipefail

CRATE="recursive-tui"

if [[ "${RECURSIVE_TUI_TEST_PRESENCE:-1}" == "0" ]]; then
  echo "[tui-test-presence] skipped (RECURSIVE_TUI_TEST_PRESENCE=0)" >&2
  exit 0
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

# TUI source files changed on this branch vs main, plus any uncommitted edits.
TUI_SRC_CHANGED="$( {
  git diff --name-only main...HEAD 2>/dev/null || true
  git diff --name-only 2>/dev/null || true
} | grep -E "^crates/$CRATE/src/" | sort -u || true )"

if [[ -z "$TUI_SRC_CHANGED" ]]; then
  echo "[tui-test-presence] no $CRATE/src/ files changed — skip" >&2
  exit 0
fi

echo "[tui-test-presence] $CRATE/src/ changed:" >&2
echo "$TUI_SRC_CHANGED" | sed 's/^/  /' >&2

has_test_change=0

# 1. Integration tests under crates/<CRATE>/tests/ changed or added.
if {
  git diff --name-only main...HEAD 2>/dev/null || true
  git diff --name-only 2>/dev/null || true
} | grep -qE "^crates/$CRATE/tests/"; then
  echo "[tui-test-presence] found integration-test change under crates/$CRATE/tests/" >&2
  has_test_change=1
fi

# 2. The PTY harness or its tests changed.
if {
  git diff --name-only main...HEAD 2>/dev/null || true
  git diff --name-only 2>/dev/null || true
} | grep -qE "^crates/tui-pty-harness/"; then
  echo "[tui-test-presence] found tui-pty-harness change" >&2
  has_test_change=1
fi

# 3. A changed TUI src file adds test markers (#[test] / #[cfg(test)] / mod tests).
#    We look at the *added* lines of the diff so deleting tests doesn't count.
while IFS= read -r f; do
  [[ -n "$f" ]] || continue
  added="$( {
    git diff main...HEAD -- "$f" 2>/dev/null || true
    git diff -- "$f" 2>/dev/null || true
  } | grep -E '^\+' | grep -vE '^\+\+\+' || true )"
  if echo "$added" | grep -qE '#\[test\]|#\[cfg\(test\)\]|mod tests'; then
    echo "[tui-test-presence] found new test marker in $f" >&2
    has_test_change=1
  fi
done <<< "$TUI_SRC_CHANGED"

if [[ "$has_test_change" -eq 1 ]]; then
  echo "[tui-test-presence] PASS — test-bearing change detected" >&2
  exit 0
fi

cat >&2 <<EOF
[tui-test-presence] FAIL — $CRATE/src/ changed but no test-bearing change detected.
  Add harness tests covering the new/changed behaviour, in the SAME change:
    - in-process: a #[cfg(test)] mod tests block in the changed src file
      (use crate::harness::Harness; assert via Screen::find_row / row_has_bg_color), OR
    - integration: a new file under crates/$CRATE/tests/ (e.g. pty_regression.rs), OR
    - PTY: a case in crates/tui-pty-harness/ .
  The mutation gate (tui-mutants.sh) will still run and rejects tautological tests.
  If this change genuinely needs no new tests (pure refactor, no behaviour change),
  set RECURSIVE_TUI_TEST_PRESENCE=0 and document the reason in the journal entry.
  See .dev/skills/tui-acceptance.md.
EOF
exit 1
