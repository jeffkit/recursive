#!/usr/bin/env bash
# launch-flow.sh — 健壮启动 self-improve.flow.js，免疫 SIGHUP，自动记录日志
#
# 解决四个反复出现的坑（2026-06-15 经验沉淀）：
#   1. 直接用 & 后台启动 → shell 退出 → SIGHUP 杀死 flow 进程
#   2. 无日志文件 → flow 崩溃无任何可查证据
#   3. 忘记先 npm install → flow 启动报 "Cannot find module"
#   4. 工作树不干净（.dev/ 文件未 commit）→ withSelfModGuard 拒绝启动
#      ★ 重要：每次修改 .dev/ 文件后必须先 commit，再运行本脚本！
#
# 关于日志"冻住"的假象：
#   Node.js stdout 重定向到文件时默认全缓冲，[step N] 等日志会积压在内存。
#   进度应通过 transcript.jsonl 文件大小来判断，而非日志行数。
#   本脚本已用 stdbuf -oL（需 coreutils）或 nohup 来缓解缓冲问题。
#
# 用法（在仓库根目录执行）：
#   .dev/scripts/launch-flow.sh --goal-file .dev/goals/276-xxx.md --provider deepseek --hitl wecom
#   .dev/scripts/launch-flow.sh --run-id selfimprove-xxxx --goal-file ... --provider deepseek
#
# 所有额外参数都转发给 self-improve.flow.js。

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
FLOW_SCRIPT="$REPO_ROOT/.dev/flows/self-improve.flow.js"
LOGS_DIR="$REPO_ROOT/.flowcast/logs"
mkdir -p "$LOGS_DIR"

# ── 0. 工作树干净检查（withSelfModGuard 要求，早检测早报错）────────
DIRTY=$(git -C "$REPO_ROOT" status --porcelain 2>/dev/null | wc -l | tr -d ' ')
if [ "$DIRTY" != "0" ]; then
  echo "[launch-flow] ❌ 工作树不干净（$DIRTY 个未提交文件），withSelfModGuard 会拒绝启动。"
  echo "   请先 commit 或 stash，再运行本脚本："
  git -C "$REPO_ROOT" status --short | head -10
  exit 1
fi

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

# 默认追加 --reviewer-agent claude（若调用方已显式指定则不重复添加）
EXTRA_ARGS=()
HAS_REVIEWER=0
for arg in "$@"; do
  if [[ "$arg" == "--reviewer-agent" || "$arg" == "--reviewer-provider" || "$arg" == "--no-review" ]]; then
    HAS_REVIEWER=1; break
  fi
done
if [ "$HAS_REVIEWER" = "0" ] && command -v claude &>/dev/null; then
  EXTRA_ARGS=("--reviewer-agent" "claude")
  echo "[launch-flow] 自动附加 --reviewer-agent claude（claude CLI 可用）"
fi

# nohup + & 保活：nohup 忽略 SIGHUP，重定向到日志文件。
# 注：Node.js 重定向到文件时 stdout 是全缓冲，日志行会有延迟。
#     进度应通过 transcript.jsonl 文件大小判断，而非日志行数。
#     若安装了 GNU coreutils（brew install coreutils），可改用：
#       stdbuf -oL node "$FLOW_SCRIPT" ...  # 强制行缓冲，日志实时
STDBUF=$(command -v stdbuf 2>/dev/null || true)
if [ -n "$STDBUF" ]; then
  NODE_CMD="$STDBUF -oL node"
else
  NODE_CMD="node"
fi

nohup $NODE_CMD "$FLOW_SCRIPT" "$@" "${EXTRA_ARGS[@]}" >> "$LOG_FILE" 2>&1 &
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
