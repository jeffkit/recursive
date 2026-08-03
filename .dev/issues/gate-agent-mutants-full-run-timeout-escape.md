# Issue — `agent-mutants` gate runs FULL 4918 mutants + timeout escapes as orphan process

**状态**: ✅ 已修复（commit `efacc40`，2026-08-03，supervisor 直接提交）
**发现时间**: 2026-08-03（Goal 376 run selfimprove-1785690279657）
**影响**: 本应 diff-scope 的 `agent-mutants` gate 跑了全量 4918 mutants，40min timeout
后 cargo-mutants 逃逸成孤儿进程继续跑，flow 的 runGate await 卡死直到人工 kill

---

## 根因（两处，均已修复）

### Bug 1 — `set -- "${ARGS[@]:-}"` bash 3.2 空数组陷阱（agent/cli-mutants.sh）

`set -- "${ARGS[@]:-}"` 在 ARGS 为空数组时，`:-` 默认值语法把**空展开**替换成
**一个空字符串参数** → `$#=1, $1=""` → 后续 `elif [[ $# -gt 0 ]]` 分支被触发
（本意是走 default auto-detect 分支）→ `for f in ""` → `--file ""`（空 glob）→
cargo-mutants 变异**整个 crate**。tui-mutants.sh 用的是 `set -- ${ARGS[@]:0}`
（安全形式），agent/cli 两个脚本没对齐。

修复：两处改为 `set -- ${ARGS[@]:0}`（bash 3.2 实测：空数组 → `$#=0`，走 auto-detect）。

### Bug 2 — flowcast spawnCapture 等 `close` 而非 `exit`（flow 层卡死）

`spawnCapture` 的 Promise 等子进程 `close` 事件（**所有 stdio 流关闭**），不是
`exit`（进程退出）。gate 超时触发时，它只 SIGTERM/SIGKILL **直接子进程**（`sh -c`
的 sh）；孙进程（cargo-mutants → cargo test）逃逸成孤儿并**继续持有 stdout/stderr
管道** → `close` 永不触发 → runGate 的 await 永不 resolve → **flow 卡死**
（state 停在 `gate.<name>`，进程/tmux 都活着，看起来健康实则无进展）。
Goal 376 实测：gate timeout 配置 40min，error 事件在 86min 后才落盘（supervisor
手动 kill 孤儿 cargo-mutants 触发管道关闭才 resolve）。

修复：flow 层加 `runGateWithWatchdog`——gate 自身 timeout 后 +15s，按命令特征
（cargo-mutants / cargo mutants / *-mutants.sh）做 pgrep **基线对比**（不杀并发
worktree 的兄弟 gate），对本次新增的 pid 递归 kill 整棵进程树 → 管道关闭 →
spawnCapture `close` 触发 → runGate 正常抛 GateError(timeout) → flow 继续走
resume-fix / preserve。单测模拟：孙进程逃逸 → kill → `CLOSE_FIRED` ✓。

---

## 症状（run selfimprove-1785690279657 实测）

1. **全量跑而非 diff-scope**。`.gate-agent-mutants-output.log` 显示
   `Mutating files: `（空）+ `Found 4918 mutants to test`——而 worktree 只改了
   `src/tools/url_guard.rs`（脚本自身复现 `CHANGED=[src/tools/url_guard.rs]` 正常）。
   推测 `--file` 参数在 gate 调用路径中未生效（argv 中 `--file` 后跟了环境变量串
   `AZURE_VAG_API_KEY=...`，疑似 `$line` 变量与 env 冲突或参数传递 bug——待查）。
   后果：命中 `checkpoint.rs`/`compact/mod.rs` 等**与本次 goal 无关的预存 mutants
   debt**（MISSED），gate 红灯。

2. **timeout 逃逸卡死 flow**。gates.json 配置 `timeout: 2400000`（40min）。spawnCapture
   超时只杀 sh/bash 包装进程，cargo-mutants（孙进程）变孤儿（ppid=1）继续跑，并持有
   stdout 管道 → `runGate` 的 await 永不 resolve → flow 状态永远停在
   `gate.agent-mutants`（看起来活着：node 进程在、tmux 在、无新日志）。直到 supervisor
   手动 `kill` 孤儿进程，18:45:01Z 才落 `GATE_FAIL timeout (exit -1)`。

## 复现路径

```
launch-flow.sh --goal-file <改 src/ 的 goal> --provider deepseek
# 等 run.recursive 完成 → gate.test/clippy/fmt/e2e 全绿 → gate.agent-mutants 卡死 80+ 分钟
```

## 影响面

- 任何改 `src/` 的 goal 都会在 `gate.agent-mutants` 卡 40min（超时）+ 卡死（逃逸）
  直到人工 kill。本 cycle 靠 supervisor 介入 + 独立验证 + cherry-pick 收尾
  （commit `5e364bc`）。
- `tui-mutants`/`cli-mutants` 同结构（diff-scope + timeout），可能有同样问题，未验证。

## 修复方向（supervisor，直接 commit，不走 goal）

1. **确认 `--file` 参数为何失效**：`agent-mutants.sh` 的 CHANGED 检测在脚本内复现正常，
   但 gate 路径下 argv 异常。检查 `runGate` → `runShell` 的参数传递（flowcast
   `quality-gate.js:20-27`：`sh -c cmd` 单字符串）与 `$line` 变量在 gate 环境下的
   取值。优先在 `/tmp` 复现（lesson 17），不直接改 flow。
2. **timeout 必须杀进程树**：spawnCapture 的 SIGTERM/SIGKILL 只发直接子进程。要么
   用 `process.kill(pid, 'SIGKILL')` + `detached: true` + 负 PID 杀进程组，要么在
   gate 脚本内加 `timeout` 命令包装，或检查 flowcast 是否有 `killTree` 原语。
3. **gate 卡死要有 liveness 上报**：runGate await 卡住时 flow 无感知。可在 gate 启动
   时记录 pid，监控侧检测「gate 超时后孙进程仍存活」即上报。

## 附带发现（同 cycle）

- Goal 373 的 run `selfimprove-1785680321270` 昨天被 supervisor kill 后 state.json
  残留 `status: running`（孤儿 run 目录，无 report.md）。flow 对 kill 的 run 应标记
  terminal 状态。
- `a6e9792`（goal 375 内容）以 `preserve: watchdog: no-growth-hung` 占位消息落在
  main——lesson 6 要求的 amend 步骤当时漏了。代码内容已验证完整（cargo audit/machete
  + AWS TLS 收窄，legacy cluster 已清除），commit 消息待规范化为
  `feat(ci): Goal 375 — ...`（等下次 push 前处理，或保持现状记录之）。
