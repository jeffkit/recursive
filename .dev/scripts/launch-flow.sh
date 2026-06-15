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

# ── 3. 用 setsid 创建新会话（彻底脱离控制终端，免疫 SIGHUP）─────
#   setsid -f: fork + new session，父进程立即返回
#   nohup 作为 fallback（macOS 上 setsid 可能不在 PATH）
if command -v setsid &>/dev/null; then
  LAUNCHER="setsid -f"
else
  LAUNCHER="nohup"
fi

echo "[launch-flow] starting flow → log: $LOG_FILE"
echo "[launch-flow] args: $*"

$LAUNCHER node "$FLOW_SCRIPT" "$@" >> "$LOG_FILE" 2>&1 &
FLOW_PID=$!

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
