#!/usr/bin/env bash
# self-improve.sh — DEVELOPER-only wrapper: invoke Recursive against its own source.
#
# This script is NOT part of the shipping product. Self-improvement is a
# workflow the developer runs in their workspace; the agent itself doesn't
# know about it.
#
# Safety net:
#   - Requires a clean working tree and a baseline commit before running.
#   - On success (agent exit 0 AND post-run `cargo test` green): auto-commits
#     all changes with a descriptive message.
#   - On any failure: hard-resets to baseline. Nothing in src/ survives.
#
# Usage:
#   .dev/scripts/self-improve.sh .dev/goals/02-foo.md
#   .dev/scripts/self-improve.sh "inline goal text"
#
# Env:
#   RECURSIVE_API_KEY (required; falls back to GLM_API_KEY / MINIMAX_API_KEY)
#   RECURSIVE_API_BASE (default: MiniMax)
#   RECURSIVE_MODEL    (default: MiniMax-M2)
#   RECURSIVE_MAX_STEPS (default: 200, matches Cursor's per-turn ceiling)
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
    ls -1t "$DEV_DIR/journal"/*.md 2>/dev/null | head -1 | while read -r f; do
      echo "--- $(basename "$f") ---"
      cat "$f"
      echo ""
    done
  fi
} > "$SYSPROMPT_FILE"

# ---- Provider profiles + rotation ------------------------------------------

# Apply a named provider profile by exporting RECURSIVE_API_BASE / _MODEL /
# _API_KEY. Returns non-zero (but doesn't `exit`) if the named API-key env is
# unset, letting the caller try a fallback.
apply_provider_profile() {
  case "$1" in
    minimax)
      export RECURSIVE_API_BASE="https://api.minimaxi.com/v1"
      export RECURSIVE_MODEL="MiniMax-M2"
      export RECURSIVE_API_KEY="${MINIMAX_API_KEY:-}"
      ;;
    deepseek)
      export RECURSIVE_API_BASE="https://api.deepseek.com/v1"
      export RECURSIVE_MODEL="deepseek-chat"
      export RECURSIVE_API_KEY="${DEEPSEEK_API_KEY:-}"
      ;;
    deepseek-pro)
      export RECURSIVE_API_BASE="https://api.deepseek.com/v1"
      export RECURSIVE_MODEL="deepseek-v4-pro"
      export RECURSIVE_API_KEY="${DEEPSEEK_API_KEY:-}"
      ;;
    glm)
      export RECURSIVE_API_BASE="https://open.bigmodel.cn/api/paas/v4"
      export RECURSIVE_MODEL="glm-5.1"
      export RECURSIVE_API_KEY="${GLM_API_KEY:-}"
      ;;
    anthropic-minimax)
      export RECURSIVE_PROVIDER_TYPE="anthropic"
      export RECURSIVE_API_BASE="https://api.minimaxi.com/anthropic"
      export RECURSIVE_MODEL="MiniMax-M2"
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

export RECURSIVE_MAX_STEPS="${RECURSIVE_MAX_STEPS:-200}"
RECURSIVE_AUTO_RESUME="${RECURSIVE_AUTO_RESUME:-1}"

# Dogfood feature wiring. Each variable defaults to "exercise the
# feature so latent bugs surface during self-improve runs". Override
# in the environment if a goal needs the feature disabled.
#
# Context compaction (g31): when the transcript exceeds this many
# characters, the agent asks the model to summarize older messages.
# 200000 chars ≈ 50K tokens; large enough that easy goals never
# trigger it, but big enough that long runs (g37/g42-style) will.
export RECURSIVE_COMPACT_THRESHOLD="${RECURSIVE_COMPACT_THRESHOLD:-200000}"

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

set +e
"$BIN" --workspace . \
  --system-prompt-file "$SYSPROMPT_FILE" \
  --transcript-out "$TRANSCRIPT_OUT" \
  $PRICING_FLAG \
  --log warn \
  run "$GOAL_BODY" 2>&1 | tee -a "$LOG"
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
      --resume-from "$RESUME_FROM" "$GOAL_BODY" 2>&1 | tee -a "$LOG"
    AGENT_STATUS=${PIPESTATUS[0]}
    set -e
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

  # Token/cost from transcript (if jq available and transcript exists)
  local tokens_prompt=0 tokens_completion=0 cost_usd="0.0"
  if command -v jq >/dev/null 2>&1 && [[ -f "$TRANSCRIPT_OUT" ]]; then
    tokens_prompt="$(jq '[.messages[]?.usage?.prompt_tokens // 0] | add // 0' "$TRANSCRIPT_OUT" 2>/dev/null || echo 0)"
    tokens_completion="$(jq '[.messages[]?.usage?.completion_tokens // 0] | add // 0' "$TRANSCRIPT_OUT" 2>/dev/null || echo 0)"
    cost_usd="$(jq '.cost_usd // 0' "$TRANSCRIPT_OUT" 2>/dev/null || echo 0)"
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
batch: 36
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

      # ---- DeepSeek flash → pro auto-fallback --------------------------------
      # If the provider was 'deepseek' (flash) and RECURSIVE_DEEPSEEK_PRO_FALLBACK
      # is enabled (default: 1), automatically retry with deepseek-pro.
      DEEPSEEK_PRO_FALLBACK="${RECURSIVE_DEEPSEEK_PRO_FALLBACK:-1}"
      if [[ "$DEEPSEEK_PRO_FALLBACK" == "1" ]] \
         && [[ "${SELECTED_PROVIDER:-}" == "deepseek" ]] \
         && [[ -z "${_RECURSIVE_IS_PRO_RETRY:-}" ]]; then
        echo ""
        echo "--- AUTO-FALLBACK: deepseek flash failed, retrying with deepseek-pro ---"
        echo ""
        export RECURSIVE_PROVIDER="deepseek-pro"
        export _RECURSIVE_IS_PRO_RETRY=1
        exec "$0" "$GOAL_SOURCE"
        # exec replaces this process — below is unreachable
      fi

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
    echo ""
    echo "=== ⚠ PANIC preserved (exit ${AGENT_STATUS}); worktree left dirty for diagnosis ==="
    echo "=== journaled to ${LOG} ==="
    echo "=== inspect: git diff  |  fix: git reset --hard ${BASELINE_SHORT} ==="
    exit 2  # distinct from rolled-back (1) and success (0)
  fi
  verdict_and_exit "rolled-back" "agent exited with status ${AGENT_STATUS}"
fi

# Defence in depth: re-run cargo test from outside the agent's transcript.
if ! cargo test --quiet >/dev/null 2>&1; then
  verdict_and_exit "rolled-back" "post-agent cargo test failed"
fi

# ---- Self-review pipeline (default ON since batch 36) ----------------------
# Runs an independent review agent against the diff. If the review rejects,
# feeds back issues for one revision round, then re-verifies.
# Set RECURSIVE_SELF_REVIEW=0 to disable for debugging or cost-sensitive runs.
if [[ "${RECURSIVE_SELF_REVIEW:-1}" == "1" ]]; then
  echo "[self-improve] running code review..."
  REVIEW_JSON=$(.dev/scripts/review-changes.sh "$SELECTED_PROVIDER" 2>/dev/null || echo '{"verdict":"approve"}')
  VERDICT=$(echo "$REVIEW_JSON" | python3 -c "import sys,json; print(json.loads(sys.stdin.read()).get('verdict','approve'))" 2>/dev/null || echo "approve")

  if [[ "$VERDICT" == "request_changes" ]]; then
    echo "[self-improve] review rejected, feeding back..."
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

    # Re-run in same worktree (up to 1 revision round)
    set +e
    "$BIN" --workspace . \
      --system-prompt-file "$SYSPROMPT_FILE" \
      --transcript-out "${TRANSCRIPT_OUT%.json}-revision.json" \
      $PRICING_FLAG \
      --log warn \
      run "$REVISION_GOAL" 2>&1 | tee -a "$LOG"
    REVISION_STATUS=${PIPESTATUS[0]}
    set -e

    if [[ "$REVISION_STATUS" -ne 0 ]]; then
      echo "[self-improve] revision agent failed, flagging for orchestrator"
      verdict_and_exit "rolled-back" "revision agent exited with status ${REVISION_STATUS}"
    fi

    # Re-verify after revision
    if ! cargo test --quiet >/dev/null 2>&1; then
      verdict_and_exit "rolled-back" "post-revision cargo test failed"
    fi

    echo "[self-improve] revision applied successfully"
  else
    echo "[self-improve] review approved"
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
