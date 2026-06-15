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

---

### 坑 4：nohup Node.js 跑 20-40 分钟后被 macOS 杀死（根本问题）

**现象**：`nohup node self-improve.flow.js` 启动后，preflight 全通过，
`run.recursive start` 写入 run log，然后约 2-5 分钟后 Node.js 进程无声消失。
`run.log.jsonl` 里只有 `run.recursive start`，没有 `done`，`.flowcast/runs/<id>/state.json`
的 `status` 停在 `"running"`，不更新。

**根本原因**：macOS 对 nohup 后台进程有 App Nap / 内存压力回收机制。
Node.js 进程在 `run.recursive` 期间：
- 父进程（Node.js/flowcast）保持 spawn 了 `recursive` 二进制的管道
- `recursive` 本身运行正常，但 Node.js 父进程在 macOS 后台被回收
- 导致 recursive 子进程也随之终止，flow 状态悬空

**验证**：单独运行 `nohup recursive run "say: ok"` ——正常，20 秒完成。
问题特指 Node.js（flowcast）层的 nohup 后台运行。

**根本解法：用 tmux**

```bash
# tmux session 是独立的 pseudo-TTY 会话，macOS 不会主动回收
tmux new-session -d -s "recursive-loop" -x 200 -y 50
tmux send-keys -t "recursive-loop" \
  "node .dev/flows/self-improve.flow.js --goal-file ... 2>&1 | tee .flowcast/logs/tmux-g<NN>.log" \
  Enter
# 验证存活
tmux ls
```

tmux session 在本次实测中连续跑完 Goal 280→281→282→283（共 40 分钟），零中断。

---

### 坑 5：resume 时 `--run-id` 必须同时带 `--goal-file`

flow 进程异常终止后，checkpoint 的 `pauseContext` 未保存 goal 文本
（进程没有走到正常暂停点）。再次运行时：

```bash
# ❌ 错误：只传 --run-id，报 "缺少 --goal 或 --goal-file"
node .dev/flows/self-improve.flow.js --run-id selfimprove-xxx

# ✅ 正确：同时传 --goal-file
node .dev/flows/self-improve.flow.js \
  --run-id selfimprove-xxx \
  --goal-file .dev/goals/<NN>-xxx.md \
  --provider deepseek
```

另外，若 `state.json` 的 `status` 是 `"running"`（进程死后未更新），
需要手动改为 `"interrupted"` 才能触发续跑逻辑：

```bash
python3 -c "
import json
with open('.flowcast/runs/<run-id>/state.json') as f: s=json.load(f)
s['status'] = 'interrupted'
with open('.flowcast/runs/<run-id>/state.json', 'w') as f: json.dump(s, f, indent=2)
print('reset to interrupted')
"
```

---

### 坑 6：orchestrator bash 脚本用 `grep -oP`，macOS BSD grep 不支持 `-P`

bash 脚本里用 `grep -oP "verdict=\S+"` 提取 verdict，在 macOS 下失败：

```
grep: invalid option -- P
usage: grep [-abcdDEFGHhIiJLlMmnOopqRSsUVvwXxZz]...
```

macOS 自带 BSD grep，不支持 Perl 正则 `-P`。

**修复**：改用 `grep -oE "verdict=[^ ]+"` 或 `sed 's/.*verdict=\([^ ]*\).*/\1/'`

---

## 有效的后台启动命令（标准做法 — tmux）

```bash
# 1. 确保工作树干净（最重要！）
git status --porcelain | wc -l   # 必须为 0

# 2. 用 tmux 启动 loop（单个 goal 或多 goal 串行）
tmux new-session -d -s "recursive-loop" -x 200 -y 50
tmux send-keys -t "recursive-loop" "
cd /Users/kongjie/projects/Recursive
node .dev/flows/self-improve.flow.js \
  --goal-file .dev/goals/<NN>-xxx.md \
  --provider deepseek \
  --reviewer-agent claude \
  --hitl wecom 2>&1 | tee .flowcast/logs/tmux-g<NN>.log
" Enter

# 3. 确认启动
sleep 5 && tmux ls

# 4. 监控进度（看 log 实时输出）
tail -f .flowcast/logs/tmux-g<NN>.log

# 5. 进入 tmux 会话查看完整输出（可选）
tmux attach -t recursive-loop
# 退出不关闭：Ctrl+B, D
```

---

## launch-flow.sh 改进清单

- [x] 改用 `stdbuf -oL node ...`（已写入脚本）
- [x] 启动前自动检测工作树是否干净，不干净时直接报错退出
- [ ] **改用 tmux 替代 nohup**（根本解法，坑 4）
- [ ] 自动检测 tmux 是否可用，可用时优先用 tmux，否则 fallback 到 nohup

---

## 已验证的 provider 情况（2026-06-15）

| Provider | 状态 | 备注 |
|---|---|---|
| DeepSeek (`deepseek-v4-pro`) | ✅ 可用 | 用 `$DEEPSEEK_API_KEY` 环境变量，不要用 JSON 里的硬编码值 |
| GLM-4.7 | ❌ 余额不足 | 需充值 |
| GLM-5.1/5.2 | ❌ 余额不足 | 需充值 |
| MiniMax | ❌ 429 Token Plan 耗尽 | 等明天重置或充值 |
| claude CLI | ✅ 可用 | 仅作 reviewer，不作 coder |
