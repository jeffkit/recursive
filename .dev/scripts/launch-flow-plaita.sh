#!/bin/zsh
# launch-flow-plaita.sh — self-improve 的 plaita 引擎启动器。
#
# 与 launch-flow.sh 同参面（--goal/--goal-file/--provider/--model/--run-id/…），
# 但走 plaita bridge（self_improve_bridge.py）而非 flowcast。launch-flow.sh 本体
# 保留不动，shadow 验证通过前 flowcast 仍是默认引擎；切换后本脚本转正。
#
# 用法（在 recursive 仓根目录；先 source 凭据 env）：
#   source ~/.issue-keeper/env.sh
#   .dev/scripts/launch-flow-plaita.sh --goal-file .dev/goals/NN-*.md --provider deepseek
#
# 输出契约与 launch-flow.sh 一致：打印 run-id / log 路径，后台 nohup 运行，
# supervisor 轮询 <repo>/.flowcast/runs/<run-id>/state.json 到终态。

set -uo pipefail
cd "$(git rev-parse --show-toplevel 2>/dev/null || pwd)" || exit 1

if [ ! -f .dev/flows/self_improve_bridge.py ]; then
  echo "ERROR: 不在 recursive 仓根（找不到 .dev/flows/self_improve_bridge.py）" >&2
  exit 1
fi

RID="selfimprove-$(date +%s)"
mkdir -p .flowcast/logs
LOG=".flowcast/logs/flow-plaita-$(date +%Y%m%dT%H%M%S).log"

export PATH="$HOME/.cargo/bin:$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
export PYTHONPATH="$HOME/projects/infra4agent/plaita:$HOME/projects/infra4agent/plaita-nodes/src"
# code 沙箱默认 10s 墙钟对自迭代完全不够（preflight.build 就要几分钟）。
# 沙箱墙钟全局放宽；真实每步超时由 shim 内层 sub_timeout 与节点 timeout 把关。
export PLAITA_SANDBOX_TIMEOUT="${PLAITA_SANDBOX_TIMEOUT:-90000}"

nohup python3 .dev/flows/self_improve_bridge.py --run-id "$RID" "$@" > "$LOG" 2>&1 &
PID=$!
echo "[launch-plaita] ✅ nohup running  pid=$PID  log=$LOG"
sleep 10
if ! kill -0 "$PID" 2>/dev/null; then
  echo "[launch-plaita] ❌ 进程已退出，日志尾部：" >&2
  tail -15 "$LOG" >&2
  exit 1
fi
echo "[launch-plaita] run-id=$RID"
echo "[launch-plaita] 监督：轮询 .flowcast/runs/$RID/state.json 到 verdict；Langfuse trace 见 localhost:3000"
tail -4 "$LOG"
