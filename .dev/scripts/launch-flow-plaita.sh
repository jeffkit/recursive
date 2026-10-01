#!/bin/zsh
# launch-flow-plaita.sh — self-improve 的 plaita 引擎启动器（v2 世代）。
#
# 2026-10-01 v1 退役（jeffkit 拍板「V1 本来就应该退役」）：本脚本改为调用
# v2 bridge（self_improve_bridge_v2.py，codeflow 库节点版——逻辑全在图里，
# 含 L1/L2 断点续跑与修复过的门禁环）。对调用方保持旧参面兼容：
#   --goal        → --goal-text
#   --provider/--model/--max-steps/--reviewer-provider/--hitl/--no-review/
#   --no-commit/--max-fix-rounds/--fixer-provider
#                 → v2 无此概念，丢弃并提示（agent 选择走 --agent/--reviewer
#                   或 SELF_IMPROVE_AGENT/SELF_IMPROVE_REVIEWER env）。
# launch-flow.sh（flowcast）保留不动，仍是回滚引擎。
#
# 用法（在 recursive 仓根目录；先 source 凭据 env）：
#   source ~/.issue-keeper/env.sh
#   .dev/scripts/launch-flow-plaita.sh --goal-file .dev/goals/NN-*.md
#
# 输出契约与 launch-flow.sh 一致：打印 run-id / log 路径，后台 nohup 运行，
# supervisor 轮询 <repo>/.flowcast/runs/<run-id>/state.json 到终态。

set -uo pipefail
cd "$(git rev-parse --show-toplevel 2>/dev/null || pwd)" || exit 1

if [ ! -f .dev/flows/self_improve_bridge_v2.py ]; then
  echo "ERROR: 不在 recursive 仓根（找不到 .dev/flows/self_improve_bridge_v2.py）" >&2
  exit 1
fi

# v1 旗标 → v2 旗标翻译
ARGS=()
dropped=()
while [ $# -gt 0 ]; do
  case "$1" in
    --goal)      ARGS+=("--goal-text"); shift ;;
    --goal-text|--goal-file|--repo|--run-id|--agent|--reviewer|--dry-run)
                 ARGS+=("$1"); shift ;;
    --provider|--model|--max-steps|--reviewer-provider|--hitl|--no-review|--no-commit|--max-fix-rounds|--fixer-provider)
                 dropped+=("$1"); shift
                 # 丢弃带值的旗标（--dry-run 型无值旗标除外）
                 case "${1-}" in --*) ;; *) [ $# -gt 0 ] && shift ;; esac ;;
    *)           ARGS+=("$1"); shift ;;
  esac
done
if [ ${#dropped[@]} -gt 0 ]; then
  echo "[launch-plaita] ⚠️ v2 无以下 v1 旗标，已忽略: ${dropped[*]}" >&2
  echo "[launch-plaita]    （agent 选择改用 --agent/--reviewer，默认 recursive+agents.json）" >&2
fi

RID="selfimprove-$(date +%s)"
mkdir -p .flowcast/logs
LOG=".flowcast/logs/flow-plaita-$(date +%Y%m%dT%H%M%S).log"

export PATH="$HOME/.cargo/bin:$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
export PYTHONPATH="$HOME/projects/infra4agent/plaita:$HOME/projects/infra4agent/plaita-nodes/src"
# code 沙箱默认 10s 墙钟对自迭代完全不够（preflight.build 就要几分钟）。
# 沙箱墙钟全局放宽；真实每步超时由 shim 内层 sub_timeout 与节点 timeout 把关。
export PLAITA_SANDBOX_TIMEOUT="${PLAITA_SANDBOX_TIMEOUT:-90000}"
# v2 的 agent 名走 agents.json（keeper 管线同款）：裸调时默认 glm53-flash，
# 与 recursive 自迭代历史（v1 deepseek/glm 直配）解耦——换模型改 agents.json。
export SELF_IMPROVE_AGENT="${SELF_IMPROVE_AGENT:-glm53-flash}"
export SELF_IMPROVE_REVIEWER="${SELF_IMPROVE_REVIEWER:-glm53-flash}"

nohup python3 .dev/flows/self_improve_bridge_v2.py "${ARGS[@]}" --run-id "$RID" > "$LOG" 2>&1 &
PID=$!
echo "[launch-plaita] ✅ nohup running  pid=$PID  log=$LOG"
sleep 10
if ! kill -0 "$PID" 2>/dev/null; then
  echo "[launch-plaita] ❌ 进程已退出，日志尾部：" >&2
  tail -15 "$LOG" >&2
  exit 1
fi
echo "[launch-plaita] run-id=$RID"
echo "[launch-plaita] 监督：轮询 .flowcast/runs/$RID/state.json 到 verdict"
tail -4 "$LOG"
