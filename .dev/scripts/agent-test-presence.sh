#!/usr/bin/env bash
# agent-test-presence.sh — fast "did you write tests?" gate for recursive-agent.
#
# Mirrors tui-test-presence.sh for the main kernel crate (`src/`).
# If a change touches product source under `src/` but adds NO test-bearing
# code, fail fast before the expensive agent-mutants gate.
#
# "Test-bearing" means ANY of:
#   - a new/changed file under `tests/` (integration / invariant tests)
#   - a changed `src/**` file whose diff adds `#[test]`, `#[cfg(test)]`,
#     or a `mod tests` block
#
# Excluded from the "src changed" trigger (same as agent-mutants.sh):
#   src/weixin/**, src/test_util.rs
#
# Exit codes:
#   0  — no agent src changed (skip), OR a test-bearing change was detected
#   1  — agent src changed but no test-bearing change detected
#
# Opt out: `RECURSIVE_AGENT_TEST_PRESENCE=0` (document reason in journal).
set -euo pipefail

if [[ "${RECURSIVE_AGENT_TEST_PRESENCE:-1}" == "0" ]]; then
  echo "[agent-test-presence] skipped (RECURSIVE_AGENT_TEST_PRESENCE=0)" >&2
  exit 0
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

SRC_CHANGED="$( {
  git diff --name-only main...HEAD 2>/dev/null || true
  git diff --name-only 2>/dev/null || true
} | grep "^src/" | grep -v "^src/weixin\|^src/test_util" | sort -u || true )"

if [[ -z "$SRC_CHANGED" ]]; then
  echo "[agent-test-presence] no recursive-agent src/ files changed — skip" >&2
  exit 0
fi

echo "[agent-test-presence] src/ changed:" >&2
echo "$SRC_CHANGED" | sed 's/^/  /' >&2

has_test_change=0

# 1. Integration / invariant tests under tests/ changed or added.
if {
  git diff --name-only main...HEAD 2>/dev/null || true
  git diff --name-only 2>/dev/null || true
} | grep -qE "^tests/"; then
  echo "[agent-test-presence] found integration-test change under tests/" >&2
  has_test_change=1
fi

# 2. A changed src file adds test markers.
while IFS= read -r f; do
  [[ -n "$f" ]] || continue
  added="$( {
    git diff main...HEAD -- "$f" 2>/dev/null || true
    git diff -- "$f" 2>/dev/null || true
  } | grep -E '^\+' | grep -vE '^\+\+\+' || true )"
  if echo "$added" | grep -qE '#\[test\]|#\[cfg\(test\)\]|mod tests'; then
    echo "[agent-test-presence] found new test marker in $f" >&2
    has_test_change=1
  fi
done <<< "$SRC_CHANGED"

if [[ "$has_test_change" -eq 1 ]]; then
  echo "[agent-test-presence] PASS — test-bearing change detected" >&2
  exit 0
fi

cat >&2 <<EOF
[agent-test-presence] FAIL — src/ changed but no test-bearing change detected.
  Add unit/integration tests covering the new/changed behaviour in the SAME change:
    - in-file: a #[cfg(test)] mod tests block with #[test] cases, OR
    - integration: a change under tests/ .
  The mutation gate (agent-mutants.sh) will still run and rejects tautological tests.
  If this change genuinely needs no new tests (pure refactor), set
  RECURSIVE_AGENT_TEST_PRESENCE=0 and document the reason in the journal entry.
EOF
exit 1
