# Manual edit: flow default max-steps

**Date**: 2026-06-15
**Goal**: self-improve.flow.js 不再落到 config.rs 的 32 步默认
**Files touched**: `.dev/flows/self-improve.flow.js`, `.dev/flows/SELF_IMPROVE.md`, `.claude/skills/recursive-loop/SKILL.md`
**Tests added**: none (flow e2e unchanged)
**Notes**: g275 首轮只 commit 了 AuditKey 别名，因未传 `--budget` 时 flow 不注入 RECURSIVE_MAX_STEPS，agent 在 32 步探索阶段触顶。现默认 200（对齐 self-improve.sh），BudgetExceeded 后 flow 仍 auto-resume 一次 → 有效约 400 步。新增 `--max-steps` CLI；`--budget` 保留为别名。
