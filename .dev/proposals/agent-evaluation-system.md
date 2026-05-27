# Proposal: Agent Self-Iteration Evaluation System

> **Status**: Draft — for discussion
> **Created**: 2026-05-27

## Problem

We run agents to write code, but lack systematic measurement of:
1. Are they actually producing good code?
2. Is the system improving over time?
3. Which configurations (provider, goal size, goal specificity) work best?
4. Is the review pipeline worth its cost?

Without data, we can't tune the system.

## Proposed Metrics Framework

### Layer 1: Per-Run Metrics (raw data, auto-collected)

Collected automatically by `self-improve.sh` on every run:

```yaml
# .dev/metrics/<run-id>.yaml
run_id: "20260527T030157Z-22494"
goal_tag: "session-jsonl-writer"
goal_id: 107
provider: deepseek
model: deepseek-chat
batch: 35

# Outcome
outcome: committed | rolled_back | no_changes
exit_reason: success | budget_exceeded | stuck | provider_error

# Effort
steps_used: 28
steps_budget: 200
tokens_prompt: 2698008
tokens_completion: 8858
cache_hit_rate: 0.951
cost_usd: 0.1146
wall_time_seconds: 150

# Code quality (computed post-run)
files_changed: 1
lines_added: 460
lines_removed: 0
tests_added: 7
test_pass: true
clippy_clean: true

# Review (if review pipeline enabled)
review_verdict: approve | request_changes | null
review_rounds: 0
review_issues_found: 0
review_issues_severity: []
orchestrator_override: null  # approve | reject | null
```

### Layer 2: Per-Goal Metrics (aggregated across retries)

```yaml
# .dev/metrics/goals/107.yaml
goal_id: 107
goal_tag: session-jsonl-writer
batch: 35
complexity: M  # S/M/L from roadmap

# Attempts
total_attempts: 1
successful_attempt: 1
providers_tried: [deepseek]
total_cost_usd: 0.1146

# Completeness (orchestrator assessment)
scope_sections_total: 8  # from goal spec numbered sections
scope_sections_completed: 7
completeness_score: 0.875
missing_items: ["session ID collision handling"]

# Quality (orchestrator assessment)
correctness_score: 9  # 0-10
architecture_score: 9
test_quality_score: 8
style_score: 8
overall_grade: A  # A/B/C/D/F

# Follow-ups needed
follow_up_goals: []
```

### Layer 3: Provider Performance (aggregated across goals)

```yaml
# .dev/metrics/providers/deepseek.yaml (auto-generated)
provider: deepseek
model: deepseek-chat
total_runs: 45
success_rate: 0.82  # committed / total
no_changes_rate: 0.05  # "nothing to do" failures
avg_steps: 32
avg_cost_usd: 0.09
avg_completeness: 0.78

# By goal complexity
by_complexity:
  S: { success_rate: 0.95, avg_cost: 0.04 }
  M: { success_rate: 0.80, avg_cost: 0.10 }
  L: { success_rate: 0.55, avg_cost: 0.18 }

# Strengths/weaknesses
strong_at: ["single-file edits", "test writing", "bug fixes"]
weak_at: ["multi-file refactors", "complex API design"]
```

### Layer 4: System-Level Health (weekly dashboard)

```
Batch 35 Summary (2026-05-27)
═══════════════════════════════
Goals attempted:  7 (107-113)
Goals completed:  3 (107, 110, 111)
Goals in-flight:  0
Goals failed:     4 (108×2, 111×1, 112×2)
Success rate:     43% (3/7 attempts) → 60% after retry

Total cost:       $0.52
Cost per success: $0.17

Provider breakdown:
  DeepSeek:  5 runs, 3 success (60%)
  MiniMax:   3 runs, 0 success (0%)  ← MiniMax down?

Review pipeline: not yet enabled
Orchestrator review time: ~15 min
Issues caught by orchestrator: 2 (missing scope items)
```

## How to Collect

### Automatic (modify self-improve.sh)

At the end of each run, emit a YAML metrics file:
```bash
# After run completes, before final status message
cat > ".dev/metrics/${RUN_ID}.yaml" <<EOF
run_id: "$RUN_ID"
goal_tag: "$GOAL_TAG"
provider: "$SELECTED_PROVIDER"
outcome: "$OUTCOME"
steps_used: $STEPS
cost_usd: $COST
# ... etc
EOF
```

Most fields come from the agent's final output (tokens, steps, cost).

### Semi-automatic (orchestrator fills in)

After review, the orchestrator adds quality scores:
```bash
# Orchestrator appends to the metrics file
cat >> ".dev/metrics/${RUN_ID}.yaml" <<EOF
# Orchestrator assessment
completeness_score: 7
correctness_score: 9
review_verdict: merge_with_note
notes: "Missing tests, no size cap implementation"
EOF
```

### Aggregation (periodic script)

`.dev/scripts/metrics-report.sh` — reads all `.dev/metrics/*.yaml`,
computes provider summaries, prints the weekly dashboard.

## Evaluation Questions We Can Answer

1. **Is the system improving?** → Track success_rate and completeness_score over batches
2. **Which provider for which task?** → Route by complexity (S→flash, L→pro)
3. **Is the review pipeline worth it?** → Compare `review_issues_found` vs `orchestrator_override` rate
4. **Are goals well-specified?** → Correlate `no_changes_rate` with goal verbosity
5. **What's the ROI?** → `cost_per_successfully_merged_goal` trend over time
6. **Where does time go?** → `steps_used / steps_budget` distribution

## Implementation Priority

```
Phase 1 (now):   Manual YAML per goal (orchestrator writes after review)
Phase 2 (g114):  self-improve.sh auto-emits basic metrics
Phase 3 (later): Aggregation script + provider routing based on data
Phase 4 (later): Review pipeline metrics + A/B testing review quality
```

## Open Questions

1. Should metrics be committed to git (auditable) or gitignored (noisy)?
   → Suggest: committed, in `.dev/metrics/` (small YAML files, valuable history)

2. How to measure "correctness" objectively?
   → Short term: orchestrator judgment (0-10 scale)
   → Long term: count bugs found in follow-up goals that trace back to this code

3. Should we A/B test review agents (e.g. DeepSeek reviews MiniMax's code)?
   → Yes, eventually. Cross-provider review may catch different things.

4. How granular? Per-run or per-goal?
   → Both. Per-run for raw data, per-goal for decision-making.
