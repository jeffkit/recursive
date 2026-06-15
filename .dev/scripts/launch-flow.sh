#!/usr/bin/env bash
# launch-flow.sh — 健壮启动 self-improve.flow.js，免疫 SIGHUP，自动记录日志
#
# 解决三个反复出现的坑：
#   1. 直接用 & 后台启动 → shell 退出 → SIGHUP 杀死 flow 进程
#   2. 无日志文件 → flow 崩溃无任何可查证据
#   3. 忘记先 npm install → flow 启动报 "Cannot find module"
#
# 用法（在仓库根目录执行）：
#   .dev/scripts/launch-flow.sh --goal-file .dev/goals/276-xxx.md --provider minimax --hitl wecom
#   .dev/scripts/launch-flow.sh --run-id selfimprove-xxxx --goal-file ... --provider minimax
#
# 所有额外参数都转发给 self-improve.flow.js。

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
FLOW_SCRIPT="$REPO_ROOT/.dev/flows/self-improve.flow.js"
LOGS_DIR="$REPO_ROOT/.flowcast/logs"
mkdir -p "$LOGS_DIR"

# ── 1. 确保 npm 依赖已安装 ────────────────────────────────────────
FLOW_DIR="$REPO_ROOT/.dev/flows"
if [ ! -d "$FLOW_DIR/node_modules/flowcast" ]; then
  echo "[launch-flow] npm install (first run or package.json changed)..."
  (cd "$FLOW_DIR" && npm install --silent)
fi

# ── 2. 生成带时间戳的日志文件 ─────────────────────────────────────
TIMESTAMP="$(date +%Y%m%dT%H%M%S)"
LOG_FILE="$LOGS_DIR/flow-${TIMESTAMP}.log"

# ── 3. 彻底脱离控制终端，免疫 SIGHUP ───────────────────────────
# 策略（按优先级）：
#   a. node 内置：在 JS 中 process.setsid() — 最干净，无外部依赖
#   b. nohup + disown — macOS/Linux 均支持，nohup 忽略 SIGHUP
# 注：macOS 的 setsid(1) 命令不存在（只有 setsid(2) 系统调用），
#     Linux 的 setsid -f 虽可用但不必要——nohup + disown 已经足够。

echo "[launch-flow] starting flow → log: $LOG_FILE"
echo "[launch-flow] args: $*"

# 用 bash 子 shell + disown 双保险：子 shell 自己 disown 自己
(
  # 忽略 SIGHUP，防止父 shell 退出时信号传播
  trap '' HUP
  exec node "$FLOW_SCRIPT" "$@" >> "$LOG_FILE" 2>&1
) &
FLOW_PID=$!
disown "$FLOW_PID" 2>/dev/null || true

# ── 4. 短暂等待确认进程存活（检测立即崩溃） ──────────────────────
sleep 2
if kill -0 "$FLOW_PID" 2>/dev/null; then
  echo "[launch-flow] ✅ flow running  pid=$FLOW_PID  log=$LOG_FILE"
  echo "$FLOW_PID" > "$LOGS_DIR/last-flow.pid"
else
  echo "[launch-flow] ❌ flow exited immediately — check log:"
  tail -20 "$LOG_FILE"
  exit 1
fi

# ── 5. 持续监视（可选）：如果加了 --wait 参数则阻塞等待完成 ─────
for arg in "$@"; do
  if [ "$arg" = "--wait" ]; then
    echo "[launch-flow] --wait: tailing log (Ctrl-C to detach, flow keeps running)..."
    tail -f "$LOG_FILE" &
    TAIL_PID=$!
    wait "$FLOW_PID" || true
    kill "$TAIL_PID" 2>/dev/null || true
    echo "[launch-flow] flow finished"
    exit 0
  fi
done
