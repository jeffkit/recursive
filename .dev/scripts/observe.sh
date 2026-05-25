#!/usr/bin/env bash
# observe.sh — extract structured metrics from a journal entry.
#
# Usage:
#   .dev/scripts/observe.sh .dev/journal/run-YYYYMMDDTHHMMSSZ.md
#
# Output is markdown, suitable for piping into .dev/observations/.

set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: $0 <journal-file>" >&2
  exit 2
fi

JOURNAL="$1"
if [[ ! -f "$JOURNAL" ]]; then
  echo "error: journal not found: $JOURNAL" >&2
  exit 2
fi

BASENAME="$(basename "$JOURNAL" .md)"

# ---- Extract header metadata ----
GOAL_TAG="$(rg -m1 '^- goal tag:\s+(.+)$' -r '$1' "$JOURNAL" || echo "?")"
PROVIDER="$(rg -m1 '^- provider:\s+(.+)$' -r '$1' "$JOURNAL" || echo "?")"
MODEL="$(rg -m1 '^- model:\s+(.+)$' -r '$1' "$JOURNAL" || echo "?")"
BASELINE="$(rg -m1 '^- baseline:\s+(.+)$' -r '$1' "$JOURNAL" || echo "?")"
VERDICT="$(rg -m1 '^- verdict:\s+(.+)$' -r '$1' "$JOURNAL" || echo "?")"

# ---- Steps consumed ----
# Each "[step N] ..." line; take unique Ns.
STEPS="$(rg -o '^\[step (\d+)\]' -r '$1' "$JOURNAL" | sort -un | wc -l | tr -d ' ')"

# ---- Termination reason ----
# Take the LAST `[done after N steps] reason: X` line. On auto-resumed
# runs the first attempt's BudgetExceeded sits earlier in the journal;
# the truth of record is whatever happened at the very end. See
# self-improve.sh's auto-resume block.
REASON="$(rg '^\[done after \d+ steps\] reason: (.+)$' -r '$1' "$JOURNAL" | tail -n1)"
[[ -z "$REASON" ]] && REASON="(unknown)"

# ---- Resume detection ----
RESUMED="no"
rg -q '^--- AUTO-RESUME: ' "$JOURNAL" && RESUMED="yes"

# ---- Tool-call distribution ----
TOOL_COUNTS="$(rg -o '^\[step \d+\] -> (\w+)' -r '$1' "$JOURNAL" | sort | uniq -c | sort -rn | awk '{print "  - "$2": "$1}')"
TOTAL_TOOL_CALLS="$(rg -c '^\[step \d+\] -> ' "$JOURNAL" || echo 0)"

# ---- Error count ----
ERRORS="$(rg -c '^ERROR: ' "$JOURNAL" || echo 0)"

# ---- Anti-stuck / budget hits ----
# Anchor to the actual termination/log lines the agent emits at runtime,
# *not* code-shaped occurrences in transcribed source. Without anchoring,
# anything writing `BudgetExceeded` or `ProviderTruncated` as a Rust
# identifier inside a write_file/apply_patch body would trip these.
# Use the LAST reason (REASON above) — auto-resumed runs may have a
# transient BudgetExceeded that was recovered by the replay.
HIT_STUCK="no"
[[ "$REASON" == "Stuck" ]] && HIT_STUCK="yes"

HIT_BUDGET="no"
[[ "$REASON" == "BudgetExceeded" ]] && HIT_BUDGET="yes"

HIT_TRUNCATED="no"
[[ "$REASON" == 'ProviderStop("length")' ]] && HIT_TRUNCATED="yes"

# ---- apply_patch vs write_file ratio (patch discipline) ----
N_APPLY="$(rg -c '^\[step \d+\] -> apply_patch' "$JOURNAL" || echo 0)"
N_WRITE="$(rg -c '^\[step \d+\] -> write_file' "$JOURNAL" || echo 0)"

# ---- Emit report ----
cat <<EOF
# Run ${BASENAME}

| field | value |
| --- | --- |
| goal | \`${GOAL_TAG}\` |
| provider | ${PROVIDER} |
| model | ${MODEL} |
| baseline | ${BASELINE} |
| verdict | ${VERDICT} |
| termination reason | ${REASON} |
| steps used | ${STEPS} |
| total tool calls | ${TOTAL_TOOL_CALLS} |
| ERROR results from tools | ${ERRORS} |
| hit anti-stuck | ${HIT_STUCK} |
| hit step budget | ${HIT_BUDGET} |
| auto-resumed | ${RESUMED} |
| hit length truncation | ${HIT_TRUNCATED} |
| apply_patch invocations | ${N_APPLY} |
| write_file invocations | ${N_WRITE} |

## Tool-call distribution

${TOOL_COUNTS}

## Patch discipline

apply_patch:write_file ratio = ${N_APPLY}:${N_WRITE}.

Higher apply_patch share = the model is editing surgically. High write_file
share = the model is rewriting whole files (either because the goal asks
for new files, or because apply_patch kept failing and it fell back).

EOF
