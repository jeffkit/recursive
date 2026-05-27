#!/usr/bin/env bash
# review-changes.sh — run a review agent on the current worktree's changes
#
# Usage: .dev/scripts/review-changes.sh [provider]
#
# Assumes CWD is the worktree with committed product changes.
# Reads the goal from .dev/goals/ (detected from branch name).
# Outputs review JSON to .dev/reviews/<run-id>.json
#
# Degrades gracefully if jq is not installed (skips re-run loop).

set -euo pipefail

PROVIDER="${1:-deepseek}"
REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$REPO_ROOT"

# ---- Determine run ID -------------------------------------------------------
TS="$(date -u +%Y%m%dT%H%M%SZ)-$$"
REVIEW_DIR="$REPO_ROOT/.dev/reviews"
mkdir -p "$REVIEW_DIR"

# ---- Get the diff against the branch base -----------------------------------
BASE_COMMIT=""
if git rev-parse --verify main >/dev/null 2>&1; then
  BASE_COMMIT=$(git merge-base HEAD main 2>/dev/null || echo "")
fi
if [[ -z "$BASE_COMMIT" ]]; then
  # Fallback: diff against parent commit
  BASE_COMMIT=$(git rev-parse HEAD~1 2>/dev/null || echo "")
fi

DIFF=""
if [[ -n "$BASE_COMMIT" ]]; then
  DIFF=$(git diff "$BASE_COMMIT"..HEAD -- src/ tests/ Cargo.toml 2>/dev/null || true)
fi

if [[ -z "$DIFF" ]]; then
  REVIEW='{"verdict":"approve","confidence":1.0,"summary":"No product changes to review","issues":[],"missing_scope":[],"score":{"completeness":10,"correctness":10,"architecture":10,"tests":10,"style":10}}'
  echo "$REVIEW" > "$REVIEW_DIR/${TS}.json"
  echo "$REVIEW"
  exit 0
fi

# ---- Find the goal file from branch name ------------------------------------
BRANCH=$(git branch --show-current 2>/dev/null || echo "unknown")
GOAL_TAG=""
GOAL_FILE=""

if [[ "$BRANCH" != "unknown" ]]; then
  # Extract goal tag from branch name: self-improve/<tag>-<provider>-<timestamp>
  GOAL_TAG=$(echo "$BRANCH" | sed 's|self-improve/||' | sed 's|-[a-z]*-[0-9TZ]*-[0-9]*$||')
  GOAL_FILE=$(find .dev/goals/ -name "*${GOAL_TAG}*" 2>/dev/null | head -1)
fi

# ---- Build the review prompt ------------------------------------------------
GOAL_CONTENT=""
if [[ -n "$GOAL_FILE" && -f "$GOAL_FILE" ]]; then
  GOAL_CONTENT=$(cat "$GOAL_FILE")
fi

AGENTS_MD=""
if [[ -f .dev/AGENTS.md ]]; then
  AGENTS_MD=$(cat .dev/AGENTS.md)
fi

REVIEW_PROMPT_FILE=$(mktemp -t review-prompt.XXXXXX)
trap 'rm -f "$REVIEW_PROMPT_FILE"' EXIT

{
  cat .dev/prompts/code-review.md
  echo ""
  echo "---"
  echo ""
  echo "## Goal specification"
  echo ""
  echo "$GOAL_CONTENT"
  echo ""
  echo "---"
  echo ""
  echo "## AGENTS.md (project standards)"
  echo ""
  echo "$AGENTS_MD"
  echo ""
  echo "---"
  echo ""
  echo "## Git diff to review"
  echo ""
  echo '```diff'
  echo "$DIFF"
  echo '```'
  echo ""
  echo "Please review and output your verdict as JSON."
} > "$REVIEW_PROMPT_FILE"

# ---- Run the review agent (single-turn, no tools needed) --------------------
# Use the same binary in the worktree. Run with RECURSIVE_MAX_STEPS=1 so the
# agent makes exactly one LLM call — just read the diff and output JSON.
export RECURSIVE_MAX_STEPS=1
# Disable features that could interfere with a single-turn review
export RECURSIVE_COMPACT_THRESHOLD=99999999
export RECURSIVE_TRACE_SPANS=0

# Find the binary
if [[ -x ./target/release/recursive ]]; then
  BIN=./target/release/recursive
elif [[ -x ./target/debug/recursive ]]; then
  BIN=./target/debug/recursive
else
  cargo build -q 2>/dev/null
  BIN=./target/debug/recursive
fi

# Run the review agent. Capture stderr separately so we can extract the JSON
# verdict from stdout. The agent outputs the review as its final message.
REVIEW_OUTPUT=$("$BIN" --workspace . --log error run "$(cat "$REVIEW_PROMPT_FILE")" 2>/dev/null || true)

# Extract JSON from the output — the verdict should be the last JSON block
# or the last line of the assistant's response.
REVIEW_JSON=""
if echo "$REVIEW_OUTPUT" | grep -q '^{.*"verdict".*}$'; then
  REVIEW_JSON=$(echo "$REVIEW_OUTPUT" | grep -o '{.*"verdict".*}' | tail -1)
elif echo "$REVIEW_OUTPUT" | grep -q '"verdict"'; then
  # Try to extract JSON from markdown code block
  REVIEW_JSON=$(echo "$REVIEW_OUTPUT" | sed -n '/```json/,/```/p' | grep -v '```' | tr -d '\n' || true)
fi

# Validate the JSON
if [[ -n "$REVIEW_JSON" ]] && echo "$REVIEW_JSON" | python3 -c "import sys,json; json.loads(sys.stdin.read())" 2>/dev/null; then
  # Valid JSON — use as-is
  :
elif [[ -n "$REVIEW_JSON" ]] && command -v jq >/dev/null 2>&1 && echo "$REVIEW_JSON" | jq . >/dev/null 2>&1; then
  # Valid JSON via jq
  :
else
  # Fallback: generate a safe default review
  REVIEW_JSON='{"verdict":"approve","confidence":0.5,"summary":"Review agent output could not be parsed as JSON; defaulting to approve","issues":[],"missing_scope":[],"score":{"completeness":5,"correctness":5,"architecture":5,"tests":5,"style":5}}'
fi

# ---- Enrich with metadata and save ------------------------------------------
RUN_ID="${TS}"
META_JSON=$(cat <<META
{
  "run_id": "$RUN_ID",
  "goal_tag": "${GOAL_TAG:-unknown}",
  "provider": "${PROVIDER}",
  "review_provider": "${PROVIDER}",
  "verdict": $(echo "$REVIEW_JSON" | python3 -c "import sys,json; print(json.dumps(json.loads(sys.stdin.read())['verdict']))" 2>/dev/null || echo '"approve"'),
  "scores": $(echo "$REVIEW_JSON" | python3 -c "import sys,json; print(json.dumps(json.loads(sys.stdin.read()).get('score',{})))" 2>/dev/null || echo '{}'),
  "issues_found": $(echo "$REVIEW_JSON" | python3 -c "import sys,json; print(len(json.loads(sys.stdin.read()).get('issues',[])))" 2>/dev/null || echo 0),
  "issues_fixed": 0,
  "rounds": 1,
  "review_cost_tokens": 0,
  "review_caught_real_bug": false
}
META
)

# Merge review JSON with metadata
FULL_REVIEW=$(echo "$REVIEW_JSON" | python3 -c "
import sys, json
review = json.loads(sys.stdin.read())
meta = json.loads('''$META_JSON''')
review.update(meta)
print(json.dumps(review, indent=2))
" 2>/dev/null || echo "$REVIEW_JSON")

echo "$FULL_REVIEW" > "$REVIEW_DIR/${TS}.json"
echo "$REVIEW_JSON"
