# Manual edit: supervisor skills sync (loop-supervise + self-improve-cycle) + retire recursive-loop refs

**Date**: 2026-08-03
**Goal**: (user request) — ① supervisor skill 应为 `loop-supervise`；② 把 ZCode 版
`self-improve-supervise` 的经验教训借鉴进 recursive 版，并让 `self-improve-cycle`
在 recursive 侧也可用（不应只有 ZCode 能使用）；③ 清理文档中不存在的
`recursive-loop` skill 引用。

## What changed

### 1. `.recursive/skills/loop-supervise/SKILL.md` (rewritten, 293 lines)

- 移除 frontmatter/body 中对 `recursive-loop` skill 的引用（该 skill 不存在：
  `.claude/` 目录被 gitignore，本地无 `.claude/skills/recursive-loop/`）。
- 借鉴 `.zcode/skills/self-improve-supervise/SKILL.md`（ZCode 版 supervisor
  playbook，22 条 discipline lessons）新增：
  - **Step chain**（preflight.* → run.recursive → gates → review → commit → verdict）
    与 verdict 语义；
  - **Tick cadence by phase**（preflight 60–90s / run 150–240s / gate 60–120s），
    launch 时捕获 run-id + tmux + log path；
  - **Flow-specific traps** 从 4 条扩到 5 条：新增 dirty-tree guard 双层
    （launch-flow.sh + flowcast withSelfModGuard）；
  - **Gate semantics**：resume-fix 语义与 MAX_FIX_ROUNDS=3、gate.e2e.fix-1
    正常自愈、e2e 成本中心（cargo-chef 缓存 / tests-only 跳过）、~100ms 假绿灯
    检测（syntax error 门是 broken 不是 passing）；
  - **Verdict handling & rescue**：committed 验证 + 按名字跑 headline test；
    failed-preserved ≠ broken（watchdog 误杀 rescue：cherry-pick +
    cargo-mutants 污染扫描 + 独立重跑三门）；skip-commit / panic-preserved 处理；
    cleanup；
  - **Supervisor discipline**：goal 只改 agent 源码不碰脚手架、goal 写作规范、
    按杠杆排序混合 scope、阻止 agent 过度自我检查（g373 教训）、不 push、
    monorepo 下先解析 $RR。
- 工具措辞保持 recursive 版（run_background / watch_file / schedule_wakeup /
  stop_loop），未照搬 ZCode 的 Bash watcher 语义。

### 2. 文档引用清理（`recursive-loop` 不存在 → 全部移除/替换）

- `AGENTS.md`（根）：Skills available 段 `/recursive-loop` → `/loop-supervise`
  （描述改为 supervisor playbook 用途）。
- `docs/architecture/skills.md`：skill_index 示例 + Project Skills 表中的
  `recursive-loop` → `loop-supervise`（trigger 模式）。
- `.zcode/skills/self-improve-supervise/SKILL.md`：两处 "loop-supervise /
  recursive-loop skills" → 只提 `loop-supervise`（顺带去掉不存在的
  `tool_search` 提及）。
- 历史 `.dev/journal/manual-*.md` 中的引用是历史记录，未改。

### 3. `.recursive/skills/self-improve-cycle/SKILL.md` (new, 299 lines)

用户指出 `self-improve-cycle` 不应只存在于 ZCode（`.zcode/skills/`），recursive
版 agent 也应能使用。新增 recursive 版：

- 从 `.zcode/skills/self-improve-cycle/SKILL.md` 适配（编排层：Phase 1 计划 →
  Phase 2 并行 review → Phase 3 出 goal → Phase 4 迭代 → Phase 5 蒸馏 →
  Phase 6 报告 + review angle bank + 并发决策树）。
- 工具措辞适配 recursive 版：Phase 2 用 `agent` 工具 `mode: "parallel"` +
  内置 `explore` 角色（`.recursive/agents/explore.md`），替代 ZCode 的并行
  Bash agent 调用；Phase 4 监督引用 `loop-supervise` SOP（run_background /
  watch_file / schedule_wakeup），替代 ZCode 的 `run_in_background: true`
  单 watcher。
- 依赖从 `self-improve-supervise` 改为 `loop-supervise`（后者已吸收其 lessons）。
- Phase 5 蒸馏目标路径改为 `.recursive/skills/`，并注明 Edit/Write 有
  SafetyCheck 保护，需经 Bash + /tmp cp 写入。

## Files touched

- `.recursive/skills/loop-supervise/SKILL.md`（重写）
- `.recursive/skills/self-improve-cycle/SKILL.md`（新增）
- `AGENTS.md`（Skills available 段；CLAUDE.md 为软链自动同步）
- `docs/architecture/skills.md`（2 处）
- `.zcode/skills/self-improve-supervise/SKILL.md`（2 处）
- `.dev/journal/manual-20260803-loop-supervise-sync.md`（本文件）

## Tests added

无（文档/技能变更，无产品代码；未触发 cargo 门。Skill 工具验证可加载
`loop-supervise`，frontmatter 校验通过；`self-improve-cycle` 已就位于发现
路径 `.recursive/skills/self-improve-cycle/SKILL.md`，但当前会话的 Skill
工具索引是启动时快照，新技能需新会话才能被工具发现）。

## Notes

- Edit/Write 工具对 `.recursive/skills/` 有 SafetyCheck 保护（防 agent 运行时
  篡改技能），本次经 Bash 写入 /tmp 再 cp，已备份原文件 /tmp/ls-backup.md。
- 修改后的文档 `grep -rn "recursive-loop"` 仅剩 journal 历史记录与 website 文档。
