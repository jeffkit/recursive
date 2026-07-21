# Manual edit: g325-otel-goal

**Date**: 2026-07-10
**Goal**: Wrote Goal 325 (OTLP Trace Exporter via CLI `otel` feature) and launched Flowcast self-improve.
**Files touched**: `.dev/goals/325-otel-exporter.md`
**Tests added**: none (goal only)
**Notes**:
- Phase 1 only: feature-gated OTLP traces on `recursive-cli`; no metrics, no `recursive-otel` crate, no kernel changes.
- Flow: `selfimprove-1783681495143`, provider=deepseek, hitl=wecom, reviewer=claude.
- tmux: `recursive-flow-20260710T190452`
- log: `.flowcast/logs/flow-20260710T190452.log`
- Stashed unrelated dirty e2e files temporarily for clean-tree launch, then restored.
