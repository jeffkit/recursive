# Goal 114 — Self-Review Pipeline: automated code review before merge

**Roadmap**: Meta-improvement — orchestrator tooling

**Design principle check**:
- Implemented as: new step in `.dev/scripts/self-improve.sh` + review prompt
- Does NOT touch product code (`src/`)
- Only modifies developer tooling under `.dev/`

## Why

The executing agents (DeepSeek flash, MiniMax) produce code that varies
in quality. Currently the orchestrator must manually review every diff
before merging — expensive in tokens and latency.

Solution: introduce a **Review Agent** step that runs automatically after
the writing agent commits. The review agent is a completely independent
session with fresh context, given only:
1. The goal spec
2. The git diff of changes
3. A review checklist (from OPERATIONS.md §3.4.1)

If the review agent approves → orchestrator does a final light pass.
If the review agent rejects → the writing agent gets the feedback and
iterates (up to 2 rounds). This forms a "write → review → revise" loop
that catches most issues before human/orchestrator involvement.

## Scope (do exactly this, no more)

### 1. Review prompt template

Create `.dev/prompts/code-review.md`:

```markdown
# Code Review

You are a code reviewer for the Recursive project (a Rust coding agent).
You have been given:
1. A goal specification (what was requested)
2. A git diff (what was actually implemented)
3. The project's coding standards (AGENTS.md)

## Your task

Review the diff against these criteria and output a structured verdict.

### Criteria

**Completeness** (weight: high)
- Does the diff implement ALL numbered sections in the goal spec?
- Are all specified tests present?
- List any missing scope items.

**Correctness** (weight: high)
- Are there logic bugs in the new code?
- Are error cases handled properly (no unwrap/expect outside tests)?
- Could any code path panic or silently fail?

**Architectural fit** (weight: medium)
- Does it follow Recursive conventions (see AGENTS.md)?
- Are new public APIs well-designed?
- Any unnecessary coupling introduced?

**Test quality** (weight: medium)
- Do tests actually verify the behaviour (not just compile)?
- Are edge cases covered?
- Any flaky test patterns (env races, timing, network)?

**Style** (weight: low)
- Reasonable function sizes?
- Clear naming?
- No dead code or TODO markers left behind?

## Output format

```json
{
  "verdict": "approve" | "request_changes",
  "confidence": 0.0-1.0,
  "summary": "one sentence overall assessment",
  "issues": [
    {
      "severity": "critical" | "major" | "minor" | "nit",
      "file": "src/session.rs",
      "description": "what's wrong",
      "suggestion": "how to fix it"
    }
  ],
  "missing_scope": ["section 3 tests not implemented", ...],
  "score": {
    "completeness": 0-10,
    "correctness": 0-10,
    "architecture": 0-10,
    "tests": 0-10,
    "style": 0-10
  }
}
```

Rules:
- `verdict: "approve"` only if no critical/major issues AND completeness >= 7
- Be specific: quote file names and line context
- If unsure about something, flag it as minor, don't block
```

### 2. Review script

Create `.dev/scripts/review-changes.sh`:

```bash
#!/usr/bin/env bash
# review-changes.sh — run a review agent on the current worktree's changes
#
# Usage: .dev/scripts/review-changes.sh [provider]
#
# Assumes CWD is the worktree with committed product changes.
# Reads the goal from .dev/goals/ (detected from branch name).
# Outputs review JSON to .dev/reviews/<run-id>.json

set -euo pipefail

PROVIDER="${1:-deepseek}"
REPO_ROOT="$(git rev-parse --show-toplevel)"

# Get the diff against the branch base
BASE_COMMIT=$(git merge-base HEAD main 2>/dev/null || git rev-parse HEAD~1)
DIFF=$(git diff "$BASE_COMMIT"..HEAD -- src/ tests/ Cargo.toml)

if [[ -z "$DIFF" ]]; then
  echo '{"verdict":"approve","confidence":1.0,"summary":"No product changes to review","issues":[],"missing_scope":[],"score":{"completeness":10,"correctness":10,"architecture":10,"tests":10,"style":10}}'
  exit 0
fi

# Find the goal file from branch name
BRANCH=$(git branch --show-current)
GOAL_TAG=$(echo "$BRANCH" | sed 's|self-improve/||' | sed 's|-[a-z]*-[0-9TZ]*-[0-9]*$||')
GOAL_FILE=$(find .dev/goals/ -name "*${GOAL_TAG}*" | head -1)

# Build the review prompt
GOAL_CONTENT=""
if [[ -n "$GOAL_FILE" && -f "$GOAL_FILE" ]]; then
  GOAL_CONTENT=$(cat "$GOAL_FILE")
fi

AGENTS_MD=$(cat .dev/AGENTS.md)

REVIEW_PROMPT=$(cat <<PROMPT
$(cat .dev/prompts/code-review.md)

---

## Goal specification

$GOAL_CONTENT

---

## AGENTS.md (project standards)

$AGENTS_MD

---

## Git diff to review

\`\`\`diff
$DIFF
\`\`\`

Please review and output your verdict as JSON.
PROMPT
)

# Run the review agent (single-turn, no tools needed)
export RECURSIVE_MAX_STEPS=1
# Use the same binary in the worktree
cargo run --quiet -- run "$REVIEW_PROMPT" 2>/dev/null | tail -1
```

### 3. Integration into self-improve.sh

After the writing agent commits successfully, before declaring
`=== ✓ committed`:

```bash
# Run review agent
echo "[self-improve] running code review..."
REVIEW_JSON=$(.dev/scripts/review-changes.sh "$SELECTED_PROVIDER")
VERDICT=$(echo "$REVIEW_JSON" | jq -r '.verdict')

if [[ "$VERDICT" == "request_changes" ]]; then
  echo "[self-improve] review rejected, feeding back..."
  # Extract issues and re-run agent with review feedback
  ISSUES=$(echo "$REVIEW_JSON" | jq -r '.issues[] | "- [\(.severity)] \(.file): \(.description). Fix: \(.suggestion)"')
  REVISION_GOAL="The code reviewer found issues with your implementation. Please fix these:

$ISSUES

Original goal for reference:
$(cat "$GOAL_FILE")

Fix ONLY the issues listed above. Do not rewrite other code."

  # Re-run in same worktree (up to 1 revision round)
  RECURSIVE_MAX_STEPS=100 cargo run -- run "$REVISION_GOAL"
  # ... re-check cargo test, clippy, etc.
fi
```

### 4. Review metadata logging

Save review results to `.dev/reviews/<run-id>.json` for tracking:

```json
{
  "run_id": "20260527T030157Z-22494",
  "goal_tag": "session-jsonl-writer",
  "provider": "deepseek",
  "review_provider": "deepseek",
  "verdict": "approve",
  "scores": { ... },
  "issues_found": 2,
  "issues_fixed": 2,
  "rounds": 1,
  "review_cost_tokens": 12000,
  "review_caught_real_bug": true
}
```

Over time this data shows:
- What % of reviews are useful (catch real bugs vs. false positives)?
- Which providers produce code that passes review on first try?
- Is the review agent itself reliable (does orchestrator override it)?

### 5. Orchestrator's reduced role

After the review pipeline passes, the orchestrator only needs to:
1. Glance at the review JSON scores
2. Spot-check one or two specific changes if scores < 8
3. Merge or reject

Expected token savings: ~70% reduction in orchestrator review time.

## Acceptance

- `.dev/scripts/review-changes.sh` runs and produces valid JSON
- `.dev/prompts/code-review.md` exists
- `self-improve.sh` has the review integration (behind a flag:
  `RECURSIVE_SELF_REVIEW=1`, default off initially)
- `.dev/reviews/` directory with at least one test output
- OPERATIONS.md §3.4 updated to reference the review pipeline

## Notes for the agent

- This is a META goal — it modifies `.dev/` tooling, NOT product code.
- The review agent runs with `RECURSIVE_MAX_STEPS=1` — single LLM call,
  no tools. Just read the diff and output JSON.
- Start with the review flag OFF by default (`RECURSIVE_SELF_REVIEW`).
  The orchestrator will enable it after validating the review quality.
- The review script must work even if `jq` isn't installed (degrade
  gracefully, skip the re-run loop).
- Don't over-engineer the revision loop. V1: just one round of feedback.
  If it still fails review after revision, flag for orchestrator.
