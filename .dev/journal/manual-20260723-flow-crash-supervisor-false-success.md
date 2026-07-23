# Manual edit: self-improve flow 抗崩 + supervisor 假成功修复

**Date**: 2026-07-23
**Goal**: 修复 2026-07-23 g329 事故暴露的两个问题：(1) flow 因 PATH 缺 `~/.cargo/bin`
在 preflight.gate-prereqs 崩溃，却报误导性的「cargo-mutants not found」；(2) flow 崩溃后
state.json 永远停在 `running`、events.jsonl 只有 `start`，supervisor 把「死 flow」误判成
「idle/健康，No intervention needed」空转到天荒地老。

**Files touched**:
- `.dev/scripts/launch-flow.sh` — 顶部 `export PATH="$HOME/.cargo/bin:$PATH"`，根治 cargo
  不在 PATH 导致的 gate-prereqs 假报错。
- `.dev/flows/self-improve.flow.js`
  - `assertGatePrereqs`（mutant 前置段）：catch 细分 `e.code === 'ENOENT'`（cargo 缺 PATH）
    vs 非零退出（cargo-mutants 真没装），分别给出可操作报错，不再笼统怪 cargo-mutants。
  - 顶层 `await main()` 套 try/catch：任何未捕获崩溃落盘 `state.status=failed` +
    `failedStep`/`error`/`failedAt`，并 `emitEvent('fatal', {error, step, stack})`，再
    `throw err` 保留 Node 打印栈 + 非 0 退出的既有诊断行为。PauseSignal（status 已 'paused'）放行。
- `.recursive/skills/loop-supervise/SKILL.md` — 「On each wake」SOP 首步改为「先探活」：
    用 `check_background` / `pgrep -f <run-id>` / tmux pane 前台进程判活；进程没了且无终态
    事件且 state 仍 `running` → 判 **crashed/intervene**，而非 idle。新增「Crashed」处理分支。
- `.claude/skills/recursive-loop/SKILL.md` — §3.5 事件表加 `fatal`，并说明看到 `fatal` 即
    flow 已死、按崩溃处理、不要空转。

**Tests added**: none（.dev/ 脚本与 skill 文档，非产品代码；已过 `node --check` + `bash -n`）。

**Notes**:
- 根因不是 cargo-mutants 没装（`cargo mutants --version` → 27.1.0，二进制 6/29 就装好），
  是 14:50/14:53 两个 tmux session 的 PATH 没 `~/.cargo/bin`，`execFileSync('cargo', ...)`
  抛 ENOENT，catch 却报「cargo-mutants not found」——方向误导。14:55/15:07 的 session PATH 对，
  就过了。
- 事故现场：run `selfimprove-1784789352900`（goal 329）崩在 preflight.gate-prereqs，
  state.json 停在 `running`，supervisor TUI `loop: idle` 盯着它的 events.jsonl 空转。
  后续 run #3（goal 343，TUI 粘贴 bug）反而跑完 commit 了——正是那个未修的粘贴 bug 把
  用户的多行 `/loop` prompt 提前提交，导致 goal 选错成 343。
- 修复后：flow 崩溃必落终态（④）+ supervisor 必探活（②），双保险消除「假成功」。
  ① 让 PATH 不再依赖父 shell，③ 让 PATH 类问题自诊断。
- ⚠️ launch-flow.sh 有「工作树干净」预检：这些 .dev/ 改动必须 commit 后才能再跑 flow，
  否则 withSelfModGuard 拒启。当前有一个 g329 活 flow（run 1784790464886）在 gate.test，
  已过 preflight，不受 main checkout dirty 影响。
