#!/usr/bin/env bash
# self-improve.sh — DEVELOPER-only wrapper: invoke Recursive against its own source.
#
# This script is NOT part of the shipping product. Self-improvement is a
# workflow the developer runs in their workspace; the agent itself doesn't
# know about it.
#
# Safety net:
#   - Requires a clean working tree and a baseline commit before running.
#   - On success (agent exit 0 AND post-run `cargo test` green AND
#     `cargo clippy --all-targets --all-features -- -D warnings` green):
#     auto-commits all changes with a descriptive message and prints
#     a "READY TO LAND" pointer for the operator.
#   - On any failure: hard-resets to baseline. Nothing in src/ survives.
#
# Usage:
#   .dev/scripts/self-improve.sh .dev/goals/02-foo.md
#   .dev/scripts/self-improve.sh "inline goal text"
#
# Env:
#   RECURSIVE_API_KEY (required; falls back to GLM_API_KEY / MINIMAX_API_KEY)
#   RECURSIVE_API_BASE (default: MiniMax)
#   RECURSIVE_MODEL    (default: MiniMax-M3)
#   RECURSIVE_MAX_STEPS (default: 0 = unlimited; set N to cap steps per pass)
#   RECURSIVE_NO_COMMIT (set to 1 to skip the auto-commit step)
#   RECURSIVE_AUTO_RESUME (default: 1; set to 0 to disable). When the
#                         first agent attempt exits with BudgetExceeded,
#                         the wrapper auto-replays from the saved
#                         transcript with the same goal exactly once.
#                         Effective ceiling = MAX_STEPS × 2 on hard
#                         goals (400 with the default); simple goals
#                         pay no extra cost.
#
#   RECURSIVE_PROVIDER       — force a named profile for this run: minimax | deepseek
#   RECURSIVE_PROVIDERS=a,b  — auto-rotate across a comma-separated list, persisting
#                              the last one used to .dev/.last-provider. Each
#                              invocation picks the next profile in the cycle.
#
# Profiles map a short name to (API_BASE, MODEL, API_KEY env). Add new ones in
# the `apply_provider_profile` function below.

set -euo pipefail

# Resolve repo root (two levels up: .dev/scripts/ -> .dev/ -> repo root).
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DEV_DIR="$REPO_ROOT/.dev"
cd "$REPO_ROOT"

# Create runtime directories the script writes to. Several of these
# are .gitignore'd (so a fresh worktree does NOT have them); the
# script must mkdir -p before any write. Observed on g263 (first
# run after the E2E hard-gate fix): verdict_and_exit's
# failure-context write crashed with "No such file or directory"
# because the rolled-back path did not mkdir .dev/runs/ first.
mkdir -p "$DEV_DIR/runs" "$DEV_DIR/journal" "$DEV_DIR/metrics" "$DEV_DIR/observations"

if [[ $# -lt 1 ]]; then
  echo "usage: $0 <goal-file-or-text>" >&2
  exit 2
fi

# Resolve the goal: file content if exists, else literal text.
if [[ -f "$1" ]]; then
  GOAL_SOURCE="$1"
  GOAL_BODY="$(cat "$1")"
  GOAL_TAG="$(basename "$1" | sed -E 's/^[0-9]+-//; s/\.md$//')"
else
  GOAL_SOURCE="<inline>"
  GOAL_BODY="$1"
  GOAL_TAG="inline"
fi

# ---- Complexity hint -------------------------------------------------------
# If the goal file contains "## Complexity: hard" (case-insensitive),
# escalate to the pro-tier model and double the step budget — unless the
# caller has already set these explicitly. The marker is advisory: respect
# any explicit RECURSIVE_PROVIDER/RECURSIVE_PROVIDERS/RECURSIVE_MAX_STEPS
# the caller has chosen, and emit a single combined log line.
if echo "$GOAL_BODY" | grep -qiE '^##[[:space:]]*Complexity:[[:space:]]*hard'; then
  COMPLEXITY_HARD=0
  PRO_TIER_ESCALATED=0
  DOUBLE_BUDGET=0
  if [[ -z "${RECURSIVE_PROVIDER:-}" && -z "${RECURSIVE_PROVIDERS:-}" ]]; then
    export RECURSIVE_PROVIDER="deepseek-pro"
    PRO_TIER_ESCALATED=1
  fi
  if [[ -z "${RECURSIVE_MAX_STEPS:-}" ]]; then
    export RECURSIVE_MAX_STEPS=400
    DOUBLE_BUDGET=1
  fi
  if [[ $PRO_TIER_ESCALATED -eq 1 || $DOUBLE_BUDGET -eq 1 ]]; then
    echo "[self-improve] Complexity: hard — using pro tier, 400 steps"
  fi
  COMPLEXITY_HARD=1
fi

# ---- Git safety net pre-flight ---------------------------------------------

if ! git rev-parse --verify HEAD >/dev/null 2>&1; then
  echo "error: no baseline commit. Commit the current state first so failures can roll back." >&2
  exit 2
fi
BASELINE_HEAD="$(git rev-parse HEAD)"
BASELINE_SHORT="$(git rev-parse --short HEAD)"

if [[ -n "$(git status --porcelain)" ]]; then
  echo "error: working tree dirty. Commit or stash before running self-improve." >&2
  git status --short >&2
  exit 2
fi

# ---- EXIT trap: force-reset the worktree on abnormal exit ------------------
# Background: g262 minimax run died with a network error at step 141.
# The script's verdict_and_exit "rolled-back" path was reached and
# the journal claim "rolled back" was written, but the actual
# `git reset --hard $BASELINE_HEAD` did NOT run — the script
# exited before reaching it, leaving the worktree dirty (modified
# src/multi.rs + untracked journal). The next run of self-improve.sh
# refused to launch because the tree was dirty, and the operator
# had to manually `cd .worktrees/... && git checkout . && git clean -fd`.
#
# The fix: a single EXIT trap that runs on ANY exit (normal,
# `set -e` failure, signal). Three flag-based exceptions:
#   - _CLEANUP_DONE: set by verdict_and_exit() once it has fully
#     handled the cleanup (committed, rolled-back) — back off.
#   - _INTENTIONAL_DIRTY: set by skip-commit / panic-preserved
#     before exit — the worktree is meant to stay dirty for
#     diagnosis, do not touch it.
#   - _WORKTREE_BASELINE empty: pre-flight failure (e.g. no
#     baseline commit, dirty working tree) — there is nothing
#     to reset to. Back off.
# Anything else is an abnormal exit: force-reset to baseline and
# commit the journal (if any) so the worktree is clean for the
# next launch.
_INTENTIONAL_DIRTY=0
_CLEANUP_DONE=0
_WORKTREE_BASELINE=""

cleanup_on_exit() {
  local exit_code=$?
  [[ "$_CLEANUP_DONE"     -eq 1 ]] && return 0
  [[ "$_INTENTIONAL_DIRTY" -eq 1 ]] && return 0
  [[ -z "$_WORKTREE_BASELINE"   ]] && return 0

  echo "!! self-improve.sh: abnormal exit (code $exit_code); force-resetting to $_WORKTREE_BASELINE" >&2
  cd "$REPO_ROOT" 2>/dev/null || cd /
  if ! git reset --hard "$_WORKTREE_BASELINE" --quiet 2>/dev/null; then
    echo "!! self-improve.sh: git reset --hard FAILED; worktree may still be dirty" >&2
    return 1
  fi
  # Best-effort: also clean untracked files (target/, .dev/journal/, etc.)
  # that the agent may have created. The worktree is meant to be
  # reusable for the next run, so leaving artifacts is unhelpful.
  git clean -fd --quiet 2>/dev/null || true
  if [[ -f "$LOG" ]]; then
    git add "$LOG" 2>/dev/null || true
    git commit --quiet -m "dev: journal — abnormal-exit cleanup (code $exit_code) on $(basename "$LOG")" 2>/dev/null || true
  fi
}

trap 'cleanup_on_exit' EXIT
# Note: _WORKTREE_BASELINE is set later, right before the agent is
# launched. This ensures that pre-flight failures (provider selection,
# tool checks, sysprompt build) leave _WORKTREE_BASELINE empty and
# the trap backs off — no spurious `git reset`/`git clean` of the
# user's main checkout or freshly-created worktree.

# ---- Build system prompt ----------------------------------------------------

SYSPROMPT_FILE="$(mktemp -t recursive-sysprompt.XXXXXX)"
trap 'rm -f "$SYSPROMPT_FILE"' EXIT

{
  echo "You are Recursive, a Rust coding agent operating on your OWN source code."
  echo "Tools: read_file, write_file, list_dir, run_shell. Sandboxed to workspace."
  echo ""
  # ---- Inject failure context from previous attempt (if any) -----------------
  FAILURE_CTX_FILE="$DEV_DIR/runs/${GOAL_TAG}-failure-context.md"
  if [[ -f "$FAILURE_CTX_FILE" ]]; then
    echo "=== IMPORTANT: Previous attempt of this goal FAILED ==="
    cat "$FAILURE_CTX_FILE"
    echo ""
    echo "=== END previous failure context ==="
    echo ""
    # Remove after injection so it's only used once
    rm -f "$FAILURE_CTX_FILE"
  fi
  echo "=== .dev/AGENTS.md (project contract) ==="
  cat "$DEV_DIR/AGENTS.md"
  echo ""
  if [[ -d "$DEV_DIR/journal" && -n "$(ls -A "$DEV_DIR/journal" 2>/dev/null || true)" ]]; then
    # Only the single most recent journal entry, to keep system-prompt size
    # bounded. Earlier 3-entry context blew past MiniMax's response-length
    # budget on multi-step goals and the LLM truncated at step 1.
    echo "=== Most recent journal entry ==="
    latest="$(ls -1t "$DEV_DIR/journal"/*.md 2>/dev/null | head -1 || true)"
    if [[ -n "$latest" ]]; then
      echo "--- $(basename "$latest") ---"
      cat "$latest"
      echo ""
    fi
  fi
} > "$SYSPROMPT_FILE"

# ---- Provider profiles + rotation ------------------------------------------

# Apply a named provider profile by exporting RECURSIVE_API_BASE / _MODEL /
# _API_KEY. Returns non-zero (but doesn't `exit`) if the named API-key env is
# unset, letting the caller try a fallback.
apply_provider_profile() {
  case "$1" in
    minimax)
      export RECURSIVE_PROVIDER_TYPE="openai"
      export RECURSIVE_API_BASE="https://api.minimaxi.com/v1"
      export RECURSIVE_MODEL="MiniMax-M3"
      export RECURSIVE_API_KEY="${MINIMAX_API_KEY:-}"
      ;;
    deepseek|deepseek-pro)
      export RECURSIVE_PROVIDER_TYPE="openai"
      export RECURSIVE_API_BASE="https://api.deepseek.com/v1"
      export RECURSIVE_MODEL="deepseek-v4-pro"
      export RECURSIVE_API_KEY="${DEEPSEEK_API_KEY:-}"
      ;;
    glm)
      export RECURSIVE_PROVIDER_TYPE="openai"
      export RECURSIVE_API_BASE="https://open.bigmodel.cn/api/paas/v4"
      export RECURSIVE_MODEL="glm-5.1"
      export RECURSIVE_API_KEY="${GLM_API_KEY:-}"
      ;;
    anthropic-minimax)
      export RECURSIVE_PROVIDER_TYPE="anthropic"
      export RECURSIVE_API_BASE="https://api.minimaxi.com/anthropic"
      export RECURSIVE_MODEL="MiniMax-M3"
      export RECURSIVE_API_KEY="${MINIMAX_API_KEY:-}"
      ;;
    anthropic-deepseek)
      export RECURSIVE_PROVIDER_TYPE="anthropic"
      export RECURSIVE_API_BASE="https://api.deepseek.com/anthropic"
      export RECURSIVE_MODEL="deepseek-chat"
      export RECURSIVE_API_KEY="${DEEPSEEK_API_KEY:-}"
      ;;
    *)
      echo "error: unknown provider profile '$1' (known: minimax | deepseek | deepseek-pro | glm | anthropic-minimax | anthropic-deepseek)" >&2
      return 2
      ;;
  esac
  [[ -n "${RECURSIVE_API_KEY:-}" ]]
}

PROVIDER_STATE_FILE="$DEV_DIR/.last-provider"

# Pick which provider profile to use for this run, in priority order:
#   1. If RECURSIVE_API_KEY is already set (legacy/manual override), do nothing.
#   2. If RECURSIVE_PROVIDER is set, apply that single profile.
#   3. If RECURSIVE_PROVIDERS is set, rotate to the next in the cycle.
#   4. Else default to 'minimax' (back-compat).
SELECTED_PROVIDER=""

if [[ -n "${RECURSIVE_API_KEY:-}" ]]; then
  # Manual override: caller set everything explicitly.
  SELECTED_PROVIDER="manual"
elif [[ -n "${RECURSIVE_PROVIDER:-}" ]]; then
  if apply_provider_profile "$RECURSIVE_PROVIDER"; then
    SELECTED_PROVIDER="$RECURSIVE_PROVIDER"
  else
    echo "error: provider '$RECURSIVE_PROVIDER' selected but its API key env is unset" >&2
    exit 2
  fi
elif [[ -n "${RECURSIVE_PROVIDERS:-}" ]]; then
  IFS=',' read -r -a CYCLE <<< "$RECURSIVE_PROVIDERS"
  LAST=""
  [[ -f "$PROVIDER_STATE_FILE" ]] && LAST="$(cat "$PROVIDER_STATE_FILE")"
  # Find LAST's index; pick (LAST+1) mod N. If LAST not in cycle, start at 0.
  NEXT_IDX=0
  for i in "${!CYCLE[@]}"; do
    if [[ "${CYCLE[$i]}" == "$LAST" ]]; then
      NEXT_IDX=$(( (i + 1) % ${#CYCLE[@]} ))
      break
    fi
  done
  SELECTED_PROVIDER="${CYCLE[$NEXT_IDX]}"
  if ! apply_provider_profile "$SELECTED_PROVIDER"; then
    echo "error: rotation picked '$SELECTED_PROVIDER' but its API key env is unset" >&2
    exit 2
  fi
  echo "$SELECTED_PROVIDER" > "$PROVIDER_STATE_FILE"
else
  if apply_provider_profile "minimax"; then
    SELECTED_PROVIDER="minimax"
  else
    echo "error: default provider 'minimax' selected but MINIMAX_API_KEY is unset" >&2
    exit 2
  fi
fi

echo "[self-improve] provider=$SELECTED_PROVIDER  model=${RECURSIVE_MODEL}" >&2

export RECURSIVE_MAX_STEPS="${RECURSIVE_MAX_STEPS:-0}"
RECURSIVE_AUTO_RESUME="${RECURSIVE_AUTO_RESUME:-1}"

# Dogfood feature wiring. Each variable defaults to "exercise the
# feature so latent bugs surface during self-improve runs". Override
# in the environment if a goal needs the feature disabled.
#
# Context compaction (g31): when the transcript exceeds this many
# characters, the agent asks the model to summarize older messages.
# Lowered from 200K to 50K chars in 2026-06 after observing that
# 4/4 self-improve runs in 2026-06 hit the MiniMax M3 context
# window limit (2013 chars per request) when the transcript
# exceeded ~50K chars. 50K ≈ 12K tokens, which is well below the
# model's window — leaving headroom for prompt caching / system
# prompt / new turns. Easy goals still fit comfortably.
export RECURSIVE_COMPACT_THRESHOLD="${RECURSIVE_COMPACT_THRESHOLD:-50000}"

# OpenTelemetry-style span timings (g42): emit a stderr line at each
# instrumented function's close with its duration. Cheap, visible,
# helps debug "where did 30 sec go" without a real OTLP exporter.
export RECURSIVE_TRACE_SPANS="${RECURSIVE_TRACE_SPANS:-1}"

# Use release build if available, else dev.
if [[ -x ./target/release/recursive ]]; then
  BIN=./target/release/recursive
else
  cargo build -q
  BIN=./target/debug/recursive
fi

# Per-run identifier. Includes the shell PID so concurrent runs in
# separate worktrees can't collide on the same TS in the same second.
TS="$(date -u +%Y%m%dT%H%M%SZ)-$$"
LOG="$DEV_DIR/journal/run-${TS}.md"
TRANSCRIPT_DIR="$DEV_DIR/transcripts"
TRANSCRIPT_OUT="$TRANSCRIPT_DIR/run-${TS}.json"
mkdir -p "$TRANSCRIPT_DIR"

# Source the redact-secrets filter. Same dir as this script.
# shellcheck disable=SC1091
source "$DEV_DIR/scripts/redact-secrets.sh"

{
  echo "# Run ${TS}"
  echo ""
  echo "- goal source: ${GOAL_SOURCE}"
  echo "- goal tag:    ${GOAL_TAG}"
  echo "- provider:    ${SELECTED_PROVIDER}"
  echo "- model:       ${RECURSIVE_MODEL}"
  echo "- baseline:    ${BASELINE_SHORT}"
  echo ""
  echo "## Goal"
  echo ""
  echo '```'
  echo "${GOAL_BODY}"
  echo '```'
  echo ""
  echo "## Agent transcript"
  echo ""
  echo '```'
} > "$LOG"

# ---- Run the agent ----------------------------------------------------------

# Pricing file for accurate cost reporting (dogfoods g51 external-pricing).
PRICING_FILE="$DEV_DIR/pricing.yaml"
PRICING_FLAG=""
[[ -f "$PRICING_FILE" ]] && PRICING_FLAG="--pricing-file $PRICING_FILE"

# From this point on, an abnormal exit (set -e failure, signal,
# process crash) should reset the worktree to the baseline and
# commit the journal. Set the baseline flag so the EXIT trap
# knows to act.
_WORKTREE_BASELINE="$BASELINE_HEAD"

set +e
"$BIN" --workspace . \
  --system-prompt-file "$SYSPROMPT_FILE" \
  --transcript-out "$TRANSCRIPT_OUT" \
  $PRICING_FLAG \
  --log warn \
  run "$GOAL_BODY" 2>&1 | redact_secrets | tee -a "$LOG"
AGENT_STATUS=${PIPESTATUS[0]}
set -e

# ---- Auto-resume on BudgetExceeded -----------------------------------------
# If the first attempt hit the step ceiling, re-seed a fresh run with the
# saved transcript and let it continue from where it left off. One resume
# only — repeated resumes hit diminishing returns and stack cost.
if [[ "$AGENT_STATUS" -ne 0 ]] \
   && [[ "$RECURSIVE_AUTO_RESUME" == "1" ]] \
   && [[ -f "$TRANSCRIPT_OUT" ]] \
   && rg -q 'reason: BudgetExceeded' "$LOG" 2>/dev/null \
   && command -v jq >/dev/null 2>&1; then
  RESUME_FROM="$(jq '.messages | length' "$TRANSCRIPT_OUT" 2>/dev/null || echo 0)"
  if [[ "$RESUME_FROM" =~ ^[0-9]+$ ]] && [[ "$RESUME_FROM" -gt 0 ]]; then
    RESUMED_TRANSCRIPT_OUT="${TRANSCRIPT_OUT%.json}-resumed.json"
    {
      echo ""
      echo "--- AUTO-RESUME: budget exceeded after ${RESUME_FROM} messages;"
      echo "    replaying with --resume-from ${RESUME_FROM} (one chance) ---"
      echo ""
    } | tee -a "$LOG"

    set +e
    "$BIN" --workspace . \
      --system-prompt-file "$SYSPROMPT_FILE" \
      --transcript-out "$RESUMED_TRANSCRIPT_OUT" \
      $PRICING_FLAG \
      --log warn \
      replay "$TRANSCRIPT_OUT" \
      --resume-from "$RESUME_FROM" "$GOAL_BODY" 2>&1 | redact_secrets | tee -a "$LOG"
    AGENT_STATUS=${PIPESTATUS[0]}
    set -e

    # After auto-resume: if cargo test passes AND there are src changes,
    # treat as success regardless of agent exit code. The agent may exit
    # non-zero (BudgetExceeded again, or ProviderStop) but the code it
    # produced before that is still valuable.
    if [[ "$AGENT_STATUS" -ne 0 ]] && git diff --quiet HEAD -- src/ 2>/dev/null; then
      : # No src changes — nothing to save, let normal failure path handle it
    elif [[ "$AGENT_STATUS" -ne 0 ]] && cargo test --quiet >/dev/null 2>&1; then
      echo ""
      echo "--- AUTO-RESUME: agent exited non-zero but cargo test passes; treating as success ---"
      AGENT_STATUS=0
    fi
  fi
fi

{
  echo '```'
  echo ""
} >> "$LOG"

# ---- Post-run verification + commit/reset ----------------------------------

# Append the ## Result footer to the journal. Always called *before* any
# git operations, so the journal change can be folded into the same commit
# (or be wiped by reset, or be on its own — but never left as dirty residue
# after the script returns).
append_result_footer() {
  local verdict="$1"
  local detail="$2"
  {
    echo "## Result"
    echo ""
    echo "- agent exit status: ${AGENT_STATUS}"
    echo "- verdict:           ${verdict}"
    [[ -n "$detail" ]] && echo "- detail:            ${detail}"
    echo "- changed files (before action):"
    echo '```'
    git status --short
    echo '```'
  } >> "$LOG"
}

# ---- Evaluation metrics (YAML) -----------------------------------------------
# Emit a structured YAML file for every run, regardless of outcome.
# This feeds the evaluation system (Layer 1: per-run raw data).
METRICS_DIR="$DEV_DIR/metrics"
mkdir -p "$METRICS_DIR"
METRICS_FILE="$METRICS_DIR/run-${TS}.yaml"

# Batch counter: read from .dev/current-batch (auto-created at 36 if missing).
# To advance: echo N > .dev/current-batch  (orchestrator / human sets this).
BATCH_FILE="$DEV_DIR/current-batch"
if [[ ! -f "$BATCH_FILE" ]]; then
  echo "36" > "$BATCH_FILE"
fi
CURRENT_BATCH="$(cat "$BATCH_FILE" 2>/dev/null || echo 36)"

emit_metrics() {
  local verdict="$1"
  local detail="$2"

  # Extract numeric data from journal
  local steps_used
  steps_used="$(rg -o '^\[step (\d+)\]' -r '$1' "$LOG" 2>/dev/null | sort -un | wc -l | tr -d ' ')"
  local total_tool_calls
  total_tool_calls="$(rg -c '^\[step [0-9]+\] -> ' "$LOG" 2>/dev/null || echo 0)"
  local error_count
  error_count="$(rg -c '^ERROR: ' "$LOG" 2>/dev/null || echo 0)"

  # Termination reason
  local term_reason
  term_reason="$(rg '^\[done after \d+ steps\] reason: (.+)$' -r '$1' "$LOG" 2>/dev/null | tail -n1)"
  [[ -z "$term_reason" ]] && term_reason="unknown"

  # Token/cost from cost.json written by CostTracker in the session dir.
  # The agent prints "session: recording to PATH" to stderr (captured in $LOG).
  # Fall back to parsing "cost: $X.XXXX" from the log if cost.json is missing.
  local tokens_prompt=0 tokens_completion=0 cost_usd="0.0"
  if command -v jq >/dev/null 2>&1; then
    local session_dir
    session_dir="$(rg -oP 'session: recording to \K\S+' "$LOG" 2>/dev/null | tail -n1)"
    if [[ -n "$session_dir" && -f "${session_dir}/cost.json" ]]; then
      tokens_prompt="$(jq '.total_usage.prompt_tokens // 0' "${session_dir}/cost.json" 2>/dev/null || echo 0)"
      tokens_completion="$(jq '.total_usage.completion_tokens // 0' "${session_dir}/cost.json" 2>/dev/null || echo 0)"
      cost_usd="$(jq '.cost_usd // 0' "${session_dir}/cost.json" 2>/dev/null || echo '0.0')"
    fi
  fi
  # Last-resort: parse "cost: $X.XXXX" printed by the agent to stderr (rg, not grep -P)
  if [[ "$cost_usd" == "0.0" || "$cost_usd" == "0" ]]; then
    local log_cost
    log_cost="$(rg -oP 'cost: \$\K[0-9.]+' "$LOG" 2>/dev/null | tail -n1)"
    [[ -n "$log_cost" ]] && cost_usd="$log_cost"
  fi

  # Code changes stats
  local files_changed=0 lines_added=0 lines_removed=0
  if [[ "$verdict" == "committed" ]]; then
    files_changed="$(git diff --stat HEAD~1 2>/dev/null | tail -1 | grep -oE '[0-9]+ file' | grep -oE '[0-9]+' || echo 0)"
    lines_added="$(git diff --numstat HEAD~1 2>/dev/null | awk '{s+=$1}END{print s+0}' || echo 0)"
    lines_removed="$(git diff --numstat HEAD~1 2>/dev/null | awk '{s+=$2}END{print s+0}' || echo 0)"
  fi

  # Test stats
  local tests_pass="true"
  [[ "$verdict" == "rolled-back" ]] && tests_pass="false"

  # Self-review data
  local review_verdict="null" review_rounds=0
  if [[ "${RECURSIVE_SELF_REVIEW:-1}" == "1" ]]; then
    if rg -q '\[self-improve\] review approved' "$LOG" 2>/dev/null; then
      review_verdict="approve"
    elif rg -q '\[self-improve\] review rejected' "$LOG" 2>/dev/null; then
      review_verdict="request_changes"
      review_rounds=1
    fi
  fi

  # Wall time: difference between run start TS and now
  local wall_time_seconds=0
  local start_epoch end_epoch
  if date --version >/dev/null 2>&1; then
    # GNU date
    start_epoch="$(date -d "${TS%-*}" +%s 2>/dev/null || echo 0)"
    end_epoch="$(date +%s)"
  else
    # BSD date (macOS)
    end_epoch="$(date +%s)"
    # Approximate: use the file creation time of $LOG
    start_epoch="$(stat -f %B "$LOG" 2>/dev/null || echo "$end_epoch")"
  fi
  wall_time_seconds=$(( end_epoch - start_epoch ))

  cat > "$METRICS_FILE" <<YAML
# Auto-generated by self-improve.sh — do not edit manually
run_id: "${TS}"
goal_tag: "${GOAL_TAG}"
goal_source: "${GOAL_SOURCE}"
provider: "${SELECTED_PROVIDER}"
model: "${RECURSIVE_MODEL}"
batch: ${CURRENT_BATCH}
baseline: "${BASELINE_SHORT}"

# Outcome
outcome: "${verdict}"
exit_reason: "${term_reason}"
detail: "${detail}"

# Effort
steps_used: ${steps_used}
steps_budget: ${RECURSIVE_MAX_STEPS}
total_tool_calls: ${total_tool_calls}
error_count: ${error_count}
tokens_prompt: ${tokens_prompt}
tokens_completion: ${tokens_completion}
cost_usd: ${cost_usd}
wall_time_seconds: ${wall_time_seconds}

# Code quality
files_changed: ${files_changed}
lines_added: ${lines_added}
lines_removed: ${lines_removed}
test_pass: ${tests_pass}

# Review
self_review_enabled: ${RECURSIVE_SELF_REVIEW:-1}
review_verdict: ${review_verdict}
review_rounds: ${review_rounds}
YAML

  echo "[self-improve] metrics emitted to ${METRICS_FILE}" >&2
}

verdict_and_exit() {
  local verdict="$1"
  local detail="$2"

  append_result_footer "$verdict" "$detail"
  emit_metrics "$verdict" "$detail"

  # The exit-trap sees _CLEANUP_DONE=1 and backs off — this function
  # is the authoritative cleanup path. For the skip-commit verdict
  # the worktree is *intentionally* left dirty, so we set
  # _INTENTIONAL_DIRTY=1 instead and re-mark _CLEANUP_DONE=0 below.
  if [[ "$verdict" == "skip-commit" ]]; then
    _INTENTIONAL_DIRTY=1
  else
    _CLEANUP_DONE=1
  fi

  case "$verdict" in
    committed)
      # Commit log + agent output together.
      git add -A
      git commit --quiet -m "self-improve(${GOAL_TAG}): ${detail}

Baseline: ${BASELINE_SHORT}
Model:    ${RECURSIVE_MODEL}
Goal:     ${GOAL_SOURCE}
"
      # Auto-generate the structured observation for this run, alongside
      # the journal. Use a separate commit so the metrics file can be
      # regenerated later (e.g. after observe.sh evolves) without
      # rewriting the self-improve commit.
      OBS_FILE="$DEV_DIR/observations/${GOAL_TAG}-${SELECTED_PROVIDER:-unknown}-${TS}.md"
      mkdir -p "$DEV_DIR/observations"
      if "$DEV_DIR/scripts/observe.sh" "$LOG" > "$OBS_FILE" 2>/dev/null; then
        git add "$OBS_FILE"
        git commit --quiet -m "dev: observation — ${GOAL_TAG} (${SELECTED_PROVIDER:-unknown})"
      else
        rm -f "$OBS_FILE"
      fi
      echo ""
      echo "=== ✓ committed: $(git log --oneline -1) ==="
      echo "=== journaled to ${LOG} ==="
      [[ -f "$OBS_FILE" ]] && echo "=== observed at ${OBS_FILE} ==="
      echo ""
      # Surface a clear "ready to land" pointer so the operator (or
      # a follow-up agent) doesn't have to hunt for the branch /
      # worktree. The branch comes from parallel-self-improve.sh when
      # launched via parallel; for a direct invocation we fall back
      # to `git branch --show-current`.
      _READY_BRANCH="${BRANCH:-$(git branch --show-current 2>/dev/null || echo HEAD)}"
      _READY_WORKTREE="${WORKTREE_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || echo .)}"
      echo "=== READY TO LAND ==="
      echo "    branch:   ${_READY_BRANCH}"
      echo "    worktree: ${_READY_WORKTREE}"
      echo "    land:     .dev/scripts/land-self-improve.sh ${GOAL_TAG}-${SELECTED_PROVIDER:-?}-${TS}"
      echo "    merge:    git merge --no-ff ${_READY_BRANCH}"
      exit 0
      ;;
    rolled-back)
      # ---- Save failure context for retry injection ---------------------------
      # When a run fails, save the last 30 lines of agent output + the error
      # reason into a .failure-context file. The next run of the same goal can
      # inject this into the system prompt to avoid repeating the same mistake.
      FAILURE_CTX_FILE="$DEV_DIR/runs/${GOAL_TAG}-failure-context.md"
      {
        echo "## Previous Attempt Failed"
        echo ""
        echo "- Provider: ${SELECTED_PROVIDER:-unknown}"
        echo "- Model: ${RECURSIVE_MODEL:-unknown}"
        echo "- Reason: ${detail}"
        echo "- Timestamp: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
        echo ""
        echo "### Last 30 lines of agent output:"
        echo '```'
        tail -30 "$LOG" 2>/dev/null || echo "(no log available)"
        echo '```'
        echo ""
        echo "### Guidance for retry:"
        echo "- Do NOT repeat the approach that caused this failure."
        echo "- If the error was a compilation error, fix it before proceeding."
        echo "- If output was truncated, use smaller patches (apply_patch, not write_file)."
      } > "$FAILURE_CTX_FILE"

      git reset --hard "${BASELINE_HEAD}" --quiet
      # Re-create the journal entry post-reset (reset wiped it).
      mkdir -p "$DEV_DIR/journal"
      cat > "$LOG.tmp" <<EOF
# Run ${TS} (ROLLED BACK)

- goal source: ${GOAL_SOURCE}
- model:       ${RECURSIVE_MODEL}
- baseline:    ${BASELINE_SHORT}
- verdict:     rolled-back
- detail:      ${detail}
EOF
      mv "$LOG.tmp" "$LOG"
      # Re-emit metrics (reset wiped the original).
      mkdir -p "$METRICS_DIR"
      emit_metrics "$verdict" "$detail"
      # Commit the failed-run journal + metrics so the tree is clean.
      git add "$LOG" "$METRICS_FILE"
      git commit --quiet -m "dev: journal — rolled-back run ${TS} (${GOAL_TAG}: ${detail})"
      echo ""
      echo "=== ✗ rolled back to ${BASELINE_SHORT} (${detail}); journal committed ==="
      echo "=== journaled to ${LOG} ==="

      exit 1
      ;;
    skip-commit)
      echo ""
      echo "=== ✓ agent succeeded but RECURSIVE_NO_COMMIT=1, leaving working tree dirty ==="
      echo "=== journaled to ${LOG} ==="
      exit 0
      ;;
  esac
}

if [[ "$AGENT_STATUS" -ne 0 ]]; then
  # Distinguish infrastructure panics from normal agent failures.
  # Panics (Rust's default exit code on panic is 101; signals add 128+N):
  #   101 = Rust panic, 134 = SIGABRT, 139 = SIGSEGV, 137 = SIGKILL
  # Normal agent failures (budget exceeded, stuck, etc.) exit with 1.
  # On panic: preserve the worktree for diagnosis instead of hard-resetting.
  if [[ "$AGENT_STATUS" -eq 101 ]] || [[ "$AGENT_STATUS" -ge 128 ]]; then
    append_result_footer "panic-preserved" "agent panicked (exit ${AGENT_STATUS})"
    emit_metrics "panic" "agent panicked (exit ${AGENT_STATUS})"
    # Commit journal + metrics without resetting — preserve the code state
    git add "$LOG" "$METRICS_FILE" 2>/dev/null || true
    git commit --quiet -m "dev: journal — PANIC run ${TS} (${GOAL_TAG}: exit ${AGENT_STATUS})" 2>/dev/null || true
    # Tell the EXIT trap to back off: this path is *intentionally*
    # leaving the worktree dirty (src/ + journal) for diagnosis.
    _INTENTIONAL_DIRTY=1
    echo ""
    echo "=== ⚠ PANIC preserved (exit ${AGENT_STATUS}); worktree left dirty for diagnosis ==="
    echo "=== journaled to ${LOG} ==="
    echo "=== inspect: git diff  |  fix: git reset --hard ${BASELINE_SHORT} ==="
    exit 2  # distinct from rolled-back (1) and success (0)
  fi
  verdict_and_exit "rolled-back" "agent exited with status ${AGENT_STATUS}"
fi

# Defence in depth: re-run cargo test from outside the agent's transcript.
if ! cargo test --quiet 2>/tmp/cargo-test-errors.log; then
  # ---- Resume-based retry on test failure ------------------------------------
  # Instead of immediate rollback, give the agent one chance to fix its own
  # mistakes by resuming the conversation with the test error as context.
  # This is more effective than starting fresh because the agent retains its
  # full understanding of the codebase changes it made.
  if [[ "${RECURSIVE_RESUME_ON_FAILURE:-1}" == "1" ]] \
     && [[ -f "$TRANSCRIPT_OUT" ]] \
     && [[ -z "${_RECURSIVE_RESUME_ATTEMPTED:-}" ]] \
     && command -v jq >/dev/null 2>&1; then
    export _RECURSIVE_RESUME_ATTEMPTED=1
    RESUME_FROM="$(jq '.messages | length' "$TRANSCRIPT_OUT" 2>/dev/null || echo 0)"
    if [[ "$RESUME_FROM" =~ ^[0-9]+$ ]] && [[ "$RESUME_FROM" -gt 0 ]]; then
      # Build the fix prompt with actual error output
      TEST_ERRORS="$(tail -40 /tmp/cargo-test-errors.log)"
      FIX_PROMPT="Your code changes caused cargo test to FAIL. Here are the errors:

\`\`\`
${TEST_ERRORS}
\`\`\`

Please fix the compilation/test errors. Do NOT start over — fix the specific issues above."

      RESUMED_TRANSCRIPT_OUT="${TRANSCRIPT_OUT%.json}-fix.json"
      {
        echo ""
        echo "--- RESUME-FIX: cargo test failed, resuming agent to fix errors ---"
        echo ""
      } | tee -a "$LOG"

      set +e
      "$BIN" --workspace . \
        --system-prompt-file "$SYSPROMPT_FILE" \
        --transcript-out "$RESUMED_TRANSCRIPT_OUT" \
        $PRICING_FLAG \
        --log warn \
        replay "$TRANSCRIPT_OUT" \
        --resume-from "$RESUME_FROM" "$FIX_PROMPT" 2>&1 | redact_secrets | tee -a "$LOG"
      FIX_STATUS=${PIPESTATUS[0]}
      set -e

      # Re-check cargo test after the fix attempt
      if [[ "$FIX_STATUS" -eq 0 ]] && cargo test --quiet >/dev/null 2>&1; then
        echo "[self-improve] RESUME-FIX: tests pass after fix ✓"
        # Continue to smoke gate / review as normal
      else
        echo "[self-improve] RESUME-FIX: still failing after fix attempt"
        verdict_and_exit "rolled-back" "post-agent cargo test failed (resume-fix also failed)"
      fi
    else
      verdict_and_exit "rolled-back" "post-agent cargo test failed"
    fi
  else
    verdict_and_exit "rolled-back" "post-agent cargo test failed"
  fi
fi

# ---- Clippy check (-D warnings) ----------------------------------------------
# Defence in depth #3: enforce `cargo clippy --all-targets --all-features
# -- -D warnings`. AGENTS.md and CLAUDE.md both name clippy as a
# mandatory quality gate, and unused-import / needless-borrow / etc.
# lints are exactly the kind of regression a `cargo test` pass hides
# (warnings don't fail `cargo test`). Without this gate, agents that
# skip their own clippy step will land code that the post-land
# clippy run (i.e. land-self-improve.sh) catches only at merge time.
#
# Observed: g262 (deepseek-pro run 20260607T090826Z) committed with
# 2 unused imports in tests/agent_team_integration.rs that
# `cargo test` accepted; only the land script's clippy gate caught
# them. Adding this gate here means the agent gets one resume-fix
# chance to clean up the warnings before we declare the run green.
#
# Disable with RECURSIVE_CLIPPY_CHECK=0 only if a goal genuinely
# needs to land clippy-dirty code (very rare — almost certainly a
# bug in the goal spec).
if [[ "${RECURSIVE_CLIPPY_CHECK:-1}" == "1" ]] \
   && ! cargo clippy --all-targets --all-features -- -D warnings \
        >/tmp/cargo-clippy-errors.log 2>&1; then
  # ---- Resume-based retry on clippy failure -----------------------------------
  # Same pattern as the cargo test gate: give the agent one chance to fix
  # its own lint warnings by resuming the conversation with the clippy
  # errors as context. Mechanical lints (needless_borrow, redundant_clone)
  # are usually a one-line fix; unused imports the agent can drop without
  # needing a follow-up compile cycle.
  if [[ "${RECURSIVE_RESUME_ON_FAILURE:-1}" == "1" ]] \
     && [[ -f "$TRANSCRIPT_OUT" ]] \
     && [[ -z "${_RECURSIVE_CLIPPY_RESUME_ATTEMPTED:-}" ]] \
     && command -v jq >/dev/null 2>&1; then
    export _RECURSIVE_CLIPPY_RESUME_ATTEMPTED=1
    RESUME_FROM="$(jq '.messages | length' "$TRANSCRIPT_OUT" 2>/dev/null || echo 0)"
    if [[ "$RESUME_FROM" =~ ^[0-9]+$ ]] && [[ "$RESUME_FROM" -gt 0 ]]; then
      CLIPPY_ERRORS="$(tail -60 /tmp/cargo-clippy-errors.log)"
      CLIPPY_FIX_PROMPT="Your code changes caused \`cargo clippy --all-targets --all-features -- -D warnings\` to FAIL. Lint warnings are errors under \`-D warnings\`. Errors:

\`\`\`
${CLIPPY_ERRORS}
\`\`\`

Please fix the lint warnings. Mechanical fixes (needless_borrow, redundant_clone, unused_imports) are usually a one-line change. Do NOT start over — fix the specific issues above."

      RESUMED_TRANSCRIPT_OUT="${TRANSCRIPT_OUT%.json}-clippy-fix.json"
      {
        echo ""
        echo "--- CLIPPY-FIX: cargo clippy failed, resuming agent to fix warnings ---"
        echo ""
      } | tee -a "$LOG"

      set +e
      "$BIN" --workspace . \
        --system-prompt-file "$SYSPROMPT_FILE" \
        --transcript-out "$RESUMED_TRANSCRIPT_OUT" \
        $PRICING_FLAG \
        --log warn \
        replay "$TRANSCRIPT_OUT" \
        --resume-from "$RESUME_FROM" "$CLIPPY_FIX_PROMPT" 2>&1 | redact_secrets | tee -a "$LOG"
      CLIPPY_FIX_STATUS=${PIPESTATUS[0]}
      set -e

      # Re-check clippy after the fix attempt
      if [[ "$CLIPPY_FIX_STATUS" -eq 0 ]] \
         && cargo clippy --all-targets --all-features -- -D warnings \
              >/dev/null 2>&1; then
        echo "[self-improve] CLIPPY-FIX: clippy clean after fix ✓" | tee -a "$LOG"
        # Re-commit any agent edits made during the clippy-fix replay.
        if [[ -n "$(git status --porcelain)" ]]; then
          git add -A
          git commit --quiet -m "self-improve(${GOAL_TAG}): clippy-fix round"
        fi
      else
        echo "[self-improve] CLIPPY-FIX: still failing after fix attempt" | tee -a "$LOG"
        verdict_and_exit "rolled-back" "post-agent cargo clippy failed (clippy-fix also failed)"
      fi
    else
      verdict_and_exit "rolled-back" "post-agent cargo clippy failed"
    fi
  else
    verdict_and_exit "rolled-back" "post-agent cargo clippy failed"
  fi
fi

# ---- Format check (cargo fmt) -----------------------------------------------
# Defence in depth #2: enforce `cargo fmt --all -- --check` so that goals
# don't accumulate formatting debt across runs (g133 leaked unformatted code
# to main; orchestrator-notes-20260528T072202Z.md flagged this gap).
#
# Disable with RECURSIVE_FMT_CHECK=0 only if a goal genuinely needs to land
# unformatted code (rare — almost certainly a bug in the agent's edit).
if [[ "${RECURSIVE_FMT_CHECK:-1}" == "1" ]] \
   && ! cargo fmt --all -- --check >/tmp/cargo-fmt-errors.log 2>&1; then
  # fmt failures are purely cosmetic — just run cargo fmt and continue rather
  # than rolling back. No need to resume the agent; rustfmt is deterministic.
  {
    echo ""
    echo "--- FMT-CHECK: cargo fmt --all -- --check FAILED — auto-fixing ---"
    tail -20 /tmp/cargo-fmt-errors.log
    echo ""
  } | tee -a "$LOG"
  cargo fmt --all 2>&1 | redact_secrets | tee -a "$LOG"
  echo "[self-improve] FMT-CHECK: auto-fixed with cargo fmt --all ✓" | tee -a "$LOG"
fi

# ---- E2E Smoke Gate ----------------------------------------------------------
# Verify the newly-built binary actually works as an agent (not just compiles).
# Uses ArgusAI replay mode with fixtures — deterministic, no API key needed.
# Disable with RECURSIVE_SMOKE_TEST=0 for debugging or when Docker is unavailable.
#
# Invocation: uses mcp2cli to call argusai MCP tools (argus_setup, argus_run,
# argus_clean) via the argusai MCP server (npx argusai-mcp). The MCP path
# provides proper worktree isolation via isolation.namespace in e2e.yaml —
# each worktree gets its own Docker container/network, parallel runs don't collide.

# Resolve mcp2cli and argusai-mcp binary path
MCP2CLI=""
for _candidate in "$HOME/.local/bin/mcp2cli" "/usr/local/bin/mcp2cli" "/opt/homebrew/bin/mcp2cli"; do
  if [[ -x "$_candidate" ]]; then
    MCP2CLI="$_candidate"
    break
  fi
done

# Resolve argusai-mcp entry point.
# Priority:
#   1. argusai-mcp installed as a standalone global package (npm i -g argusai-mcp)
#   2. argusai-mcp bundled inside argusai-cli global install
#   3. npx (pulls from npm on first run; slower but always up-to-date)
ARGUSAI_MCP_BIN=""
_argusai_mcp_npx=""
for _npm_root in \
    "$(npm root -g 2>/dev/null)" \
    "$HOME/.local/share/fnm/node-versions"/*/installation/lib/node_modules; do
  # Standalone install takes priority
  if [[ -f "$_npm_root/argusai-mcp/dist/index.js" ]]; then
    ARGUSAI_MCP_BIN="$_npm_root/argusai-mcp/dist/index.js"
    break
  fi
  # Fallback: bundled inside argusai-cli
  if [[ -f "$_npm_root/argusai-cli/node_modules/argusai-mcp/dist/index.js" ]]; then
    ARGUSAI_MCP_BIN="$_npm_root/argusai-cli/node_modules/argusai-mcp/dist/index.js"
    break
  fi
done
# If no local install found, fall back to npx (requires network on first use)
if [[ -z "$ARGUSAI_MCP_BIN" ]] && command -v npx >/dev/null 2>&1; then
  _argusai_mcp_npx="npx argusai-mcp"
fi

# Call one argusai MCP tool via a named mcp2cli session.
# MCP server is stateful — init/setup/run/clean must share the same process.
# Usage: _argus <session-name> <tool-name> [--flag value ...]
_argus() {
  local session="$1"; shift
  "$MCP2CLI" --session "$session" "$@" 2>&1
}

# Build the MCP stdio command: prefer node <path>, fall back to npx
if [[ -n "$ARGUSAI_MCP_BIN" ]]; then
  _MCP_STDIO_CMD="node $ARGUSAI_MCP_BIN"
else
  _MCP_STDIO_CMD="$_argusai_mcp_npx"
fi

if [[ "${RECURSIVE_SMOKE_TEST:-1}" == "1" ]] \
   && [[ -n "$MCP2CLI" ]] \
   && [[ -n "$_MCP_STDIO_CMD" ]] \
   && [[ -f "e2e/e2e.yaml" ]]; then
  echo "[self-improve] running E2E smoke gate (via mcp2cli → argusai MCP)..."

  # Rebuild binary so the container gets the new code.
  cargo build -q 2>/dev/null

  # Build e2e plugins (e2e/plugins/dist/) before argus-init. The worktree
  # is a fresh git checkout — node_modules and dist/ are gitignored, so
  # `e2e.yaml`'s `plugins: - ./plugins/dist/index.js` reference will
  # otherwise fail with PLUGIN_LOAD_ERROR.
  #
  # The `argusai-core` dep in e2e/plugins/package.json is declared as
  # `file:../../../infra4agent/argusai/packages/core`. The `../../../`
  # resolves relative to e2e/plugins/. From the main repo
  # (3 levels up = repo root) it works. From a worktree
  # (e2e/plugins/ is at .worktrees/<name>/e2e/plugins/, so 3 levels up
  # = .worktrees/<name>/) it does NOT. Patch the dep to an absolute
  # path for the duration of the build, then restore the original
  # package.json. Resolve the absolute path from the git **common**
  # dir (the shared .git), so it works from any worktree.
  if [[ -d "e2e/plugins" ]] && [[ -f "e2e/plugins/package.json" ]]; then
    if [[ ! -f "e2e/plugins/dist/index.js" ]]; then
      echo "[self-improve] building e2e/plugins (dist/ missing)..."
      PLUGIN_PKG="e2e/plugins/package.json"
      PLUGIN_BAK="$(mktemp -t recursive-plugin-pkg-XXXXXX.json)"
      cp "$PLUGIN_PKG" "$PLUGIN_BAK"
      GIT_COMMON="$(git rev-parse --git-common-dir)"
      REPO_ROOT="$(cd "$GIT_COMMON/.." && pwd)"
      ARGUSAI_CORE_ABS="$REPO_ROOT/../infra4agent/argusai/packages/core"
      if [[ -d "$ARGUSAI_CORE_ABS" ]]; then
        sed -i '' "s|file:../../../infra4agent/argusai/packages/core|file:${ARGUSAI_CORE_ABS}|" "$PLUGIN_PKG"
      fi
      (cd e2e/plugins && pnpm install --no-frozen-lockfile 2>/dev/null && pnpm build 2>&1 | tail -3)
      cp "$PLUGIN_BAK" "$PLUGIN_PKG"
      # `--no-frozen-lockfile` may have rewritten pnpm-lock.yaml.
      # Restore it from git to avoid leaving a dirty worktree.
      (cd e2e/plugins && git checkout -- pnpm-lock.yaml 2>/dev/null)
      rm -f "$PLUGIN_BAK"
    fi
  fi

  # Per-worktree namespace — each run gets isolated Docker container + network.
  WORKTREE_ID="wt-$(git rev-parse --short HEAD 2>/dev/null || echo 'main')"
  export WORKTREE_ID
  E2E_PROJECT="$(pwd)/e2e"
  MCP_SESSION="argusai-$WORKTREE_ID"

  # Start a persistent MCP server session so init/setup/run share state.
  # WORKTREE_ID must be in env BEFORE session-start so the server inherits it.
  WORKTREE_ID="$WORKTREE_ID" "$MCP2CLI" --mcp-stdio "$_MCP_STDIO_CMD"     --session-start "$MCP_SESSION" >/dev/null 2>&1

  # Capture init output to a side file so e2e failures can be diagnosed.
  # Previously this was silenced (`>/dev/null 2>&1`), which made SESSION_NOT_FOUND
  # errors at argus-run time untraceable to the actual init failure.
  E2E_INIT_LOG="$(pwd)/.dev/runs/e2e-init-${MCP_SESSION}.log"
  if ! _argus "$MCP_SESSION" argus-init --project-path "$E2E_PROJECT" >"$E2E_INIT_LOG" 2>&1; then
    echo "[self-improve] E2E smoke: argus-init FAILED — see $E2E_INIT_LOG"
    cat "$E2E_INIT_LOG" | head -20
  fi
  _argus "$MCP_SESSION" argus-setup --project-path "$E2E_PROJECT" 2>&1 | tail -3

  if _argus "$MCP_SESSION" argus-run --project-path "$E2E_PROJECT" --filter "smoke" 2>&1 | grep -q '"passed"'; then
    echo "[self-improve] E2E smoke: PASSED ✓"
    _argus "$MCP_SESSION" argus-clean --project-path "$E2E_PROJECT" >/dev/null 2>&1 || true
  else
    echo "[self-improve] E2E smoke: FAILED ✗"
    SMOKE_ERRORS="$(_argus "$MCP_SESSION" argus-run --project-path "$E2E_PROJECT" --filter "smoke" 2>&1 | tail -30)"
    _argus "$MCP_SESSION" argus-clean --project-path "$E2E_PROJECT" >/dev/null 2>&1 || true
    "$MCP2CLI" --session-stop "$MCP_SESSION" >/dev/null 2>&1 || true

    # Give the agent one chance to fix the regression before rolling back.
    if [[ "${RECURSIVE_RESUME_ON_FAILURE:-1}" == "1" ]] \
       && [[ -f "$TRANSCRIPT_OUT" ]] \
       && [[ -z "${_RECURSIVE_SMOKE_RESUME_ATTEMPTED:-}" ]] \
       && command -v jq >/dev/null 2>&1; then
      export _RECURSIVE_SMOKE_RESUME_ATTEMPTED=1
      RESUME_FROM="$(jq '.messages | length' "$TRANSCRIPT_OUT" 2>/dev/null || echo 0)"
      if [[ "$RESUME_FROM" =~ ^[0-9]+$ ]] && [[ "$RESUME_FROM" -gt 0 ]]; then
        SMOKE_PROMPT="The E2E smoke suite failed after your changes. Output:

\`\`\`
${SMOKE_ERRORS}
\`\`\`

Please investigate and fix the regression. Do NOT start over — fix the specific issue above."

        SMOKE_TRANSCRIPT_OUT="${TRANSCRIPT_OUT%.json}-smoke-fix.json"
        echo "--- SMOKE-FIX: E2E smoke failed, resuming agent to fix ---" | tee -a "$LOG"
        set +e
        "$BIN" --workspace . \
          --system-prompt-file "$SYSPROMPT_FILE" \
          --transcript-out "$SMOKE_TRANSCRIPT_OUT" \
          $PRICING_FLAG \
          --log warn \
          replay "$TRANSCRIPT_OUT" \
          --resume-from "$RESUME_FROM" "$SMOKE_PROMPT" 2>&1 | redact_secrets | tee -a "$LOG"
        SMOKE_FIX_STATUS=${PIPESTATUS[0]}
        set -e

        cargo build -q 2>/dev/null
        # Re-start session for the retry run
        WORKTREE_ID="$WORKTREE_ID" "$MCP2CLI" --mcp-stdio "$_MCP_STDIO_CMD" \
          --session-start "$MCP_SESSION" >/dev/null 2>&1
        E2E_INIT_LOG="$(pwd)/.dev/runs/e2e-init-${MCP_SESSION}.log"
        if ! _argus "$MCP_SESSION" argus-init --project-path "$E2E_PROJECT" >"$E2E_INIT_LOG" 2>&1; then
          echo "[self-improve] E2E smoke: argus-init (retry) FAILED — see $E2E_INIT_LOG"
          cat "$E2E_INIT_LOG" | head -20
        fi
        _argus "$MCP_SESSION" argus-setup --project-path "$E2E_PROJECT" 2>&1 | tail -3
        if [[ "$SMOKE_FIX_STATUS" -eq 0 ]] && \
           _argus "$MCP_SESSION" argus-run --project-path "$E2E_PROJECT" --filter "smoke" 2>/dev/null | grep -q '"passed"'; then
          echo "[self-improve] SMOKE-FIX: smoke passes after fix ✓" | tee -a "$LOG"
          _argus "$MCP_SESSION" argus-clean --project-path "$E2E_PROJECT" >/dev/null 2>&1 || true
          "$MCP2CLI" --session-stop "$MCP_SESSION" >/dev/null 2>&1 || true
        else
          _argus "$MCP_SESSION" argus-clean --project-path "$E2E_PROJECT" >/dev/null 2>&1 || true
          "$MCP2CLI" --session-stop "$MCP_SESSION" >/dev/null 2>&1 || true
          echo "[self-improve] SMOKE-FIX: still failing after fix attempt" | tee -a "$LOG"
          verdict_and_exit "rolled-back" "E2E smoke test failed (smoke-fix also failed)"
        fi
      else
        verdict_and_exit "rolled-back" "E2E smoke test failed (new binary broken)"
      fi
    else
      verdict_and_exit "rolled-back" "E2E smoke test failed (new binary broken)"
    fi
  fi
  "$MCP2CLI" --session-stop "$MCP_SESSION" >/dev/null 2>&1 || true
fi

if [[ "${RECURSIVE_SMOKE_TEST:-1}" == "1" ]] && { [[ -z "$MCP2CLI" ]] || [[ -z "$_MCP_STDIO_CMD" ]]; }; then
  # ---- Hard gate: missing E2E prerequisites must fail the run ---------------
  _missing=()
  [[ -n "$MCP2CLI" ]]          || _missing+=("mcp2cli — install: uv tool install mcp2cli")
  [[ -n "$_MCP_STDIO_CMD" ]]   || _missing+=("argusai-mcp — install: npm install -g argusai-mcp")
  [[ -f "e2e/e2e.yaml" ]]      || _missing+=("e2e/e2e.yaml")
  echo "[self-improve] E2E: HARD GATE FAILED — missing prerequisites:" >&2
  printf '  - %s\n' "${_missing[@]}" >&2
  echo "[self-improve] E2E: fix the environment (install mcp2cli: uv tool install mcp2cli)" >&2
  echo "[self-improve]      then re-run self-improve.sh." >&2
  echo "[self-improve]      To skip intentionally: RECURSIVE_SMOKE_TEST=0 .dev/scripts/self-improve.sh ..." >&2
  verdict_and_exit "rolled-back" "E2E prerequisites missing: ${_missing[*]}"
fi

# ---- Self-review pipeline (default ON since batch 36) ----------------------
# Runs an independent review agent against the diff. If the review rejects,
# feeds back issues for one revision round, then re-verifies.
# Set RECURSIVE_SELF_REVIEW=0 to disable for debugging or cost-sensitive runs.
if [[ "${RECURSIVE_SELF_REVIEW:-1}" == "1" ]]; then
  MAX_REVISION_ROUNDS="${RECURSIVE_MAX_REVISION_ROUNDS:-2}"
  REVISION_ROUND=0
  VERDICT="approve"

  while [[ "$REVISION_ROUND" -lt "$MAX_REVISION_ROUNDS" ]]; do
    echo "[self-improve] running code review..."
    REVIEW_JSON=$(.dev/scripts/review-changes.sh "$SELECTED_PROVIDER" 2>/dev/null || echo '{"verdict":"approve"}')
    VERDICT=$(echo "$REVIEW_JSON" | python3 -c "import sys,json; print(json.loads(sys.stdin.read()).get('verdict','approve'))" 2>/dev/null || echo "approve")

    if [[ "$VERDICT" != "request_changes" ]]; then
      echo "[self-improve] review approved"
      break
    fi

    REVISION_ROUND=$(( REVISION_ROUND + 1 ))
    echo "[self-improve] review rejected (round ${REVISION_ROUND}/${MAX_REVISION_ROUNDS}), feeding back..."

    # Extract issues as a bullet list
    ISSUES=$(echo "$REVIEW_JSON" | python3 -c "
import sys, json
r = json.loads(sys.stdin.read())
for i in r.get('issues', []):
    print(f\"- [{i.get('severity','minor')}] {i.get('file','?')}: {i.get('description','?')}. Fix: {i.get('suggestion','?')}\")
" 2>/dev/null || echo "Review found issues (could not parse details)")

    REVISION_GOAL="The code reviewer found issues with your implementation. Please fix these:

$ISSUES

Original goal for reference:
$(cat "$GOAL_SOURCE" 2>/dev/null || echo "$GOAL_BODY")

Fix ONLY the issues listed above. Do not rewrite other code."

    set +e
    "$BIN" --workspace . \
      --system-prompt-file "$SYSPROMPT_FILE" \
      --transcript-out "${TRANSCRIPT_OUT%.json}-revision-${REVISION_ROUND}.json" \
      $PRICING_FLAG \
      --log warn \
      run "$REVISION_GOAL" 2>&1 | redact_secrets | tee -a "$LOG"
    REVISION_STATUS=${PIPESTATUS[0]}
    set -e

    if [[ "$REVISION_STATUS" -ne 0 ]]; then
      echo "[self-improve] revision agent failed, flagging for orchestrator"
      verdict_and_exit "rolled-back" "revision agent exited with status ${REVISION_STATUS}"
    fi

    # Re-verify after each revision; bail immediately on failure
    if ! cargo test --quiet >/dev/null 2>&1; then
      verdict_and_exit "rolled-back" "post-revision cargo test failed (round ${REVISION_ROUND})"
    fi

    echo "[self-improve] revision round ${REVISION_ROUND} applied successfully"
  done

  if [[ "$VERDICT" == "request_changes" ]]; then
    echo "[self-improve] review still rejecting after ${MAX_REVISION_ROUNDS} rounds — committing with warnings"
  fi
fi

# Decide whether a meaningful change happened. Journal files are this
# script's own artifact, not agent output; they must not count toward
# "the agent made changes". Without this filter, every run gets credited
# as a success purely because we wrote a transcript.
PRODUCT_CHANGES="$(git status --porcelain | grep -vE '^.. \.dev/journal/' || true)"

if [[ -z "$PRODUCT_CHANGES" ]]; then
  # Append the result footer first so it gets folded into the same commit.
  append_result_footer "skip-commit" "agent succeeded but made no product changes"
  # Still commit any journal (untracked transcript) so the tree is clean
  # for the next run.
  if [[ -n "$(git status --porcelain)" ]]; then
    git add -A
    git commit --quiet -m "dev: journal — run ${TS} (no product changes)"
  fi
  echo ""
  echo "=== ✓ agent succeeded but made no product changes; journal committed ==="
  echo "=== journaled to ${LOG} ==="
  exit 0
fi

if [[ "${RECURSIVE_NO_COMMIT:-0}" == "1" ]]; then
  verdict_and_exit "skip-commit" "RECURSIVE_NO_COMMIT=1 set"
fi

CHANGED_COUNT="$(echo "$PRODUCT_CHANGES" | wc -l | tr -d ' ')"
verdict_and_exit "committed" "${CHANGED_COUNT} files changed, cargo test green"
