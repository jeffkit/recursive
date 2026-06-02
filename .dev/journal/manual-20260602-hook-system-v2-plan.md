# Manual edit: hook-system-v2-plan

**Date**: 2026-06-02
**Goal**: 对比 fake-cc（Claude Code）hook 机制，制定 Recursive Hook System V2 完整对齐提案
**Files touched**:
- `.dev/proposals/hook-system-v2.md` — 全量 Gap 分析 + 三阶段实施方案
- `.dev/goals/204-hook-events-expansion.md` — P1-1: 扩展 HookEvent（6→14 种）
- `.dev/goals/205-hook-output-format.md` — P1-2: 扩展输出格式（additionalContext/updatedInput 等）
- `.dev/goals/206-hook-settings-file.md` — P1-3: hooks.json + Matcher 过滤
- `.dev/goals/207-hook-http-type.md` — P2-1: HTTP hook 类型
- `.dev/goals/208-hook-prompt-type.md` — P2-2: Prompt hook 类型（LLM 评估）
- `.dev/goals/209-hook-async-support.md` — P2-3: async/asyncRewake/once 标志
- `.dev/goals/210-hook-tui-progress.md` — P3-1: TUI hook 进度展示
- `.worktrees/feat/hook-system-v2` — 开发分支已建立

**Tests added**: none（规划阶段）
**Notes**:
对比分析基于 fake-cc 源码（~/Downloads/fake-cc/）中的：
- src/types/hooks.ts — 类型系统
- src/schemas/hooks.ts — 配置 Schema（command/prompt/http/agent 四类型）
- src/utils/hooks/hookEvents.ts — 进度事件系统
- src/utils/hooks.ts — 执行引擎

主要 6 大 Gap：事件类型不完整（6 vs 18+）、Hook 类型单一（只有 shell）、
输出解析弱（3 个动作 vs 富字段）、无异步 Hook、配置机制原始、TUI 无 Hook 展示。

推进顺序：204 → 205 → 206 → (207/208/209 并行) → 210
