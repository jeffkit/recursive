# Manual edit: flow-background-execution

**Date**: 2026-06-15
**Goal**: 记录后台运行 self-improve.flow.js 的可靠方法
**Files touched**: `.dev/scripts/launch-flow.sh`（待改）
**Tests added**: none

---

## 问题描述

在这个 session 里，后台启动 `self-improve.flow.js` 反复失败，耗费大量调试时间。

---

## 踩的坑（按顺序）

### 坑 1：修改 .dev/ 文件忘记 commit，导致 withSelfModGuard 拒绝启动

`captureBaseline` 调用 `withSelfModGuard`，要求工作树干净。

任何对 `.dev/scripts/launch-flow.sh`、`.dev/flows/self-improve.flow.js`
等文件的修改，**必须在启动 flow 之前 commit**，否则 flow 直接报错退出：

```
withSelfModGuard: 工作树不干净，请先 commit/stash：
M .dev/scripts/launch-flow.sh
```

**教训**：每次修改 `.dev/` 下的文件后，立即 `git add && git commit`，
再运行 `launch-flow.sh`。

---

### 坑 2：Node.js stdout 缓冲 —— 后台日志看起来"冻住了"

当 Node.js 的 stdout 被重定向到文件时（非 TTY），默认启用全缓冲。
流程实际上在正常运行（LLM 调用有响应，steps 在推进），
但 `[step N] llm latency: Xms` 等日志行积压在内存缓冲区里，
不写入日志文件，导致 `tail -f` 看起来永远停在第 19 行。

容易被误判为进程已死，触发不必要的重启。

**正确判断方法**：不看日志行数，而是看：
- `wc -c session/transcript.jsonl` ——有增长说明 agent 在工作
- `kill -0 $PID` ——进程是否存活
- `ls -la .flowcast/runs/<run-id>/` ——state.json 修改时间

**修复方案**：用 `stdbuf -oL node ...` 强制行缓冲（需先 `brew install coreutils`）。
已在 `launch-flow.sh` 里加入，但本次 session 受坑 1 影响无法生效。

---

### 坑 3：macOS 旧版 screen 无法创建 socket

`/usr/bin/screen`（version 4.00.03，2006年）在某些 macOS 版本下
`screen -dmS` 静默失败，`screen -ls` 显示 "No Sockets found"。
Homebrew 版 `screen` 也遇到类似问题（权限/tmp 目录不同）。

**可靠替代方案**：`nohup node ... > log 2>&1 &`，虽然没有 screen 那样的重连能力，
但对无人值守的单次 flow 运行来说足够稳定。

---

## 有效的后台启动命令（标准做法）

```bash
# 1. 确保工作树干净（最重要！）
git status --porcelain | wc -l   # 必须为 0

# 2. 用 nohup 启动，日志写到固定位置
cd /Users/kongjie/projects/Recursive
LOG=.flowcast/logs/flow-g<NN>-$(date +%H%M).log
nohup node .dev/flows/self-improve.flow.js \
  --goal-file .dev/goals/<NN>-xxx.md \
  --provider deepseek \
  --reviewer-agent claude \
  --hitl wecom > "$LOG" 2>&1 &
echo "PID: $!  LOG: $LOG"

# 3. 8-10 秒后确认存活
sleep 10 && kill -0 $! && echo "alive" || echo "dead, check $LOG"

# 4. 监控进度（看 transcript 增长，不看日志行数）
watch -n 30 'ls -la ~/.recursive/workspaces/*/sessions/*/*/transcript.jsonl | tail -3'
```

---

## launch-flow.sh 的改进清单（待做）

- [ ] 改用 `stdbuf -oL node ...`（已写入脚本但需测试）
- [ ] 启动前自动检测工作树是否干净，不干净时直接报错退出
- [ ] PID 写入 `.flowcast/logs/last-flow.pid` 并记录启动时间

---

## 已验证的 provider 情况（2026-06-15）

| Provider | 状态 | 备注 |
|---|---|---|
| DeepSeek (`deepseek-v4-pro`) | ✅ 可用 | 用 `$DEEPSEEK_API_KEY` 环境变量，不要用 JSON 里的硬编码值 |
| GLM-4.7 | ❌ 余额不足 | 需充值 |
| GLM-5.1/5.2 | ❌ 余额不足 | 需充值 |
| MiniMax | ❌ 429 Token Plan 耗尽 | 等明天重置或充值 |
| claude CLI | ✅ 可用 | 仅作 reviewer，不作 coder |
