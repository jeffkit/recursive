#!/usr/bin/env bash
# launch-flow.sh — 健壮启动 self-improve.flow.js，免疫 SIGHUP，自动记录日志
#
# 解决五个反复出现的坑（2026-06-15 经验沉淀）：
#   1. 直接用 & 后台启动 → shell 退出 → SIGHUP 杀死 flow 进程
#   2. 无日志文件 → flow 崩溃无任何可查证据
#   3. 忘记先 npm install → flow 启动报 "Cannot find module"
#   4. 工作树不干净（.dev/ 文件未 commit）→ withSelfModGuard 拒绝启动
#      ★ 重要：每次修改 .dev/ 文件后必须先 commit，再运行本脚本！
#   5. nohup Node.js 在 macOS 跑 20-40 分钟后被系统回收（App Nap）
#      ★ 根本解法：优先用 tmux，tmux session 不受 macOS 进程回收影响
#
# 关于日志"冻住"的假象：
#   Node.js stdout 重定向到文件时默认全缓冲，[step N] 等日志会积压在内存。
#   进度应通过 transcript.jsonl 文件大小来判断，而非日志行数。
#   本脚本已用 stdbuf -oL（需 coreutils）来缓解缓冲问题。
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
TMUX_SESSION="recursive-flow-${TIMESTAMP}"

# ── 3. 默认追加跨 provider review（deepseek ↔ deepseek-pro）────────
# Claude API 已不可用，不再自动挂 --reviewer-agent claude。
# 未显式指定 reviewer 时：provider=deepseek → reviewer=deepseek-pro，
# 其它 provider → reviewer=deepseek（与实现 agent 错开）。
EXTRA_ARGS=()
HAS_REVIEWER=0
PROVIDER_NAME=""
for arg in "$@"; do
  if [[ "$arg" == "--reviewer-agent" || "$arg" == "--reviewer-provider" || "$arg" == "--no-review" ]]; then
    HAS_REVIEWER=1
  fi
done
# 提取 --provider 值（简单扫一遍；值紧跟在 flag 后）
prev=""
for arg in "$@"; do
  if [[ "$prev" == "--provider" ]]; then
    PROVIDER_NAME="$arg"
  fi
  prev="$arg"
done
if [ "$HAS_REVIEWER" = "0" ]; then
  if [[ "$PROVIDER_NAME" == "deepseek" || "$PROVIDER_NAME" == "deepseek-flash" ]]; then
    EXTRA_ARGS=("--reviewer-provider" "deepseek-pro")
  else
    EXTRA_ARGS=("--reviewer-provider" "deepseek")
  fi
  echo "[launch-flow] 自动附加 ${EXTRA_ARGS[*]}（claude reviewer 已停用；deepseek/deepseek-pro 轮换）"
fi

# ── 4. stdbuf 行缓冲（日志实时写入）──────────────────────────────
STDBUF=$(command -v stdbuf 2>/dev/null || true)
if [ -n "$STDBUF" ]; then
  NODE_CMD="$STDBUF -oL node"
else
  NODE_CMD="node"
fi

echo "[launch-flow] starting flow → log: $LOG_FILE"
echo "[launch-flow] args: $*"

# ── 5. 启动策略：优先 tmux，fallback 到 nohup ─────────────────────
# tmux session 是独立的 pseudo-TTY，macOS 不会主动回收，适合 20-40 分钟的 flow。
# nohup 对短 flow（< 5 分钟）足够，但长 flow 有被 App Nap 回收的风险。
FLOW_CMD="cd $REPO_ROOT && $NODE_CMD $FLOW_SCRIPT $* ${EXTRA_ARGS[*]:-} 2>&1 | tee $LOG_FILE; echo '[flow done]' >> $LOG_FILE"

if command -v tmux &>/dev/null; then
  tmux new-session -d -s "$TMUX_SESSION" -x 200 -y 50
  tmux send-keys -t "$TMUX_SESSION" "$FLOW_CMD" Enter
  echo "[launch-flow] ✅ tmux session: $TMUX_SESSION  log: $LOG_FILE"
  echo "[launch-flow]    attach: tmux attach -t $TMUX_SESSION"
  echo "[launch-flow]    detach: Ctrl+B, D"
  echo "$TMUX_SESSION" > "$LOGS_DIR/last-flow.tmux"
  echo "" > "$LOGS_DIR/last-flow.pid"
else
  # fallback: nohup（适合 < 5 分钟的短 flow 或 tmux 不可用时）
  echo "[launch-flow] ⚠️  tmux 不可用，fallback 到 nohup（长 flow 可能被 macOS 杀死）"
  nohup $NODE_CMD "$FLOW_SCRIPT" "$@" "${EXTRA_ARGS[@]:-}" >> "$LOG_FILE" 2>&1 &
  FLOW_PID=$!
  sleep 2
  if kill -0 "$FLOW_PID" 2>/dev/null; then
    echo "[launch-flow] ✅ nohup running  pid=$FLOW_PID  log=$LOG_FILE"
    echo "$FLOW_PID" > "$LOGS_DIR/last-flow.pid"
  else
    echo "[launch-flow] ❌ flow exited immediately — check log:"
    tail -20 "$LOG_FILE"
    exit 1
  fi
fi

# ── 6. 等待首行日志（确认 flow 实际启动，而非 tmux 静默失败）───────
echo "[launch-flow] 等待 flow 输出首行..."
for i in $(seq 1 10); do
  sleep 2
  if [ -s "$LOG_FILE" ]; then
    echo "[launch-flow] ✅ log active:"
    head -5 "$LOG_FILE"
    break
  fi
done

if [ ! -s "$LOG_FILE" ]; then
  echo "[launch-flow] ⚠️  10 秒内无日志输出，请检查："
  echo "   tmux attach -t $TMUX_SESSION  （查看实时输出）"
fi

# ── 7. --wait 模式：阻塞直到完成 ──────────────────────────────────
for arg in "$@"; do
  if [ "$arg" = "--wait" ]; then
    echo "[launch-flow] --wait: tailing log (Ctrl-C to detach, flow keeps running)..."
    tail -f "$LOG_FILE"
    exit 0
  fi
done
