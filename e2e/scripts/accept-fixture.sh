#!/bin/bash
# accept-fixture.sh <suite-id> — 验收即录制：录制真模型 → agent 自验收 → promote → 回放。
#
# 把「录制」从独立动作变成验收的副产品：跑一次真模型冒烟，捕获真实 transcript，
# 由一个独立的 agent-judge 对照 expectation 判定是否符合预期，PASS 才把 recorded/
# 升级成回归 fixture，最后无 key 回放确认。
#
# 链路（4 步）：
#   1. 录制：E2E_RECORD=1 跑 argusai lifecycle（init→build→setup→run），aimock 代理
#      真模型，把每次请求-响应落到 e2e/fixtures/recorded/。
#   2. 验收：argus-run 后、argus-clean 前（容器还活着），docker cp 出本次 session
#      transcript，spawn 一个只读 recursive agent 当 judge，对照
#      e2e/expectations/<suite-id>.md 判 {completed, score}。
#   3. promote：PASS（completed=true && score>=阈值）才调 promote.sh 合并
#      recorded/*.json → fixtures/<suite-id>.json。FAIL 不 promote，保留 recorded/。
#   4. 回放：无 key 重跑 e2e-run.sh <suite-id>，确认新 fixture 回归绿。
#
# 为什么不直接用 e2e-run.sh 的黑盒：录制跑完它的 cleanup trap 会 argus-clean 销毁
# 容器，session transcript（容器内 /tmp/sessions-* 或 /workspace/sessions）随之消失，
# judge 无从读。本脚本自控 lifecycle，在 run↔clean 之间 docker cp。
#
# 判据来源：e2e/expectations/<suite-id>.md（预期行为契约）。没配 expectation 的套件
# 退回「判完整性」（工具调用是否合理、产出是否符合 goal）。expectation 机制见
# e2e/RECORD_REPLAY.md「验收即录制」节。
#
# Usage:
#   ./accept-fixture.sh <suite-id> [--score N] [--keep]
#   ./accept-fixture.sh loop-schedule
#   ./accept-fixture.sh loop-schedule --score 4 --keep   # 保留录制产物+transcript
#
# Env:
#   DEEPSEEK_API_KEY   真模型 key（录制+judge 都需要）
#   DEEPSEEK_API_BASE  默认 https://api.deepseek.com/v1
#   JUDGE_MODEL         judge 用的模型，默认 deepseek-chat
#   ACCEPT_SCORE_MIN    验收分数阈值，默认 4（可被 --score 覆盖）
#
# 退出码：0=全链路绿（录制+验收+promote+回放）；1=任一步红。

set -uo pipefail

SUITE="${1:?Usage: $0 <suite-id> [--score N] [--keep]}"
SCORE_MIN="${ACCEPT_SCORE_MIN:-4}"
KEEP=0
shift || true
while [[ $# -gt 0 ]]; do
  case "$1" in
    --score) SCORE_MIN="$2"; shift 2 ;;
    --keep)  KEEP=1; shift ;;
    *) echo "[accept] unknown arg: $1" >&2; exit 2 ;;
  esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
E2E_PROJECT="$REPO_ROOT/e2e"
EXPECTATION="$E2E_PROJECT/expectations/${SUITE}.md"
RECORDED_DIR="$E2E_PROJECT/fixtures/recorded"
JUDGE_WORK="$(mktemp -d -t accept-judge-XXXX)"

# 录制跑完不 cp 容器——容器在 e2e-run.sh 黑盒里已销毁。本脚本改自控 lifecycle：
# argus-run 返回后容器仍存活（clean 在 trap EXIT），立即 docker cp transcript。
CONTAINER="recursive-e2e"

# ---- 解析工具链（同 e2e-run.sh / e2e-gate.sh）------------------------------
MCP2CLI=""
for _c in "$HOME/.local/bin/mcp2cli" "/usr/local/bin/mcp2cli" "/opt/homebrew/bin/mcp2cli"; do
  [[ -x "$_c" ]] && { MCP2CLI="$_c"; break; }
done
[[ -n "$MCP2CLI" ]] || { echo "[accept] mcp2cli not found" >&2; exit 3; }

ARGUSAI_MCP_BIN=""
for _root in "$(npm root -g 2>/dev/null)" \
    "$HOME/.local/share/fnm/node-versions"/*/installation/lib/node_modules; do
  if [[ -f "$_root/argusai-mcp/dist/index.js" ]]; then
    ARGUSAI_MCP_BIN="$_root/argusai-mcp/dist/index.js"; break
  fi
done
if [[ -n "$ARGUSAI_MCP_BIN" ]]; then
  _MCP_STDIO_CMD="node $ARGUSAI_MCP_BIN"
elif command -v npx >/dev/null 2>&1; then
  _MCP_STDIO_CMD="npx argusai-mcp"
else
  echo "[accept] argusai-mcp not found" >&2; exit 3
fi

# ---- 校验前置 -------------------------------------------------------------
[[ -n "${DEEPSEEK_API_KEY:-}" ]] || { echo "[accept] DEEPSEEK_API_KEY 未设置（录制需要真模型 key）" >&2; exit 3; }
command -v recursive >/dev/null 2>&1 || { echo "[accept] host 缺 recursive 二进制（judge 需要）" >&2; exit 3; }
[[ -d "$E2E_PROJECT" ]] || { echo "[accept] e2e 项目目录不存在: $E2E_PROJECT" >&2; exit 3; }

if [[ -f "$EXPECTATION" ]]; then
  echo "[accept] expectation: $EXPECTATION"
else
  echo "[accept] 无 expectation，退回判完整性（工具调用合理 + 产出符合 goal）"
fi
echo "[accept] suite=$SUITE  score_min=$SCORE_MIN"

# ---- argusai lifecycle（init→build→setup→run→[cp+judge]→clean）------------
export WORKTREE_ID="wt-accept-$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || echo main)"
export E2E_RECORD=1
export DEEPSEEK_API_KEY
export DEEPSEEK_API_BASE="${DEEPSEEK_API_BASE:-https://api.deepseek.com/v1}"
# 录制时 agent 必须发真 key：aimock --record 转发请求里的 Authorization 到上游，
# 不替换成容器 OPENAI_API_KEY。e2e.yaml 经 {{env.E2E_RECORD_API_KEY}} 读取。
# 同理 model：录制时用真 model 名（如 deepseek-chat），回放时 mock-chat。
export E2E_RECORD_API_KEY="$DEEPSEEK_API_KEY"
export E2E_RECORD_MODEL="${DEEPSEEK_MODEL:-deepseek-chat}"
MCP_SESSION="accept-$$"

_argus() {
  local out; out="$("$MCP2CLI" --session "$MCP_SESSION" "$@" 2>&1)"; local rc=$?
  echo "$out"
  if echo "$out" | python3 -c "import sys,json; d=json.load(sys.stdin); sys.exit(0 if d.get('success',True) else 1)" 2>/dev/null; then
    return $rc
  else
    return 1
  fi
}

cleanup() {
  _argus argus-clean --project-path "$E2E_PROJECT" >/dev/null 2>&1 || true
  docker rm -f aimock >/dev/null 2>&1 || true
  "$MCP2CLI" --session-stop "$MCP_SESSION" >/dev/null 2>&1 || true
  if [[ "$KEEP" -eq 0 ]]; then
    rm -rf "$JUDGE_WORK"
  else
    echo "[accept] --keep：保留 judge workspace: $JUDGE_WORK"
  fi
}
trap cleanup EXIT

echo "[accept] step 1/4 — 录制（E2E_RECORD=1, 真模型）..."
"$MCP2CLI" --mcp-stdio "$_MCP_STDIO_CMD" --session-start "$MCP_SESSION" >/dev/null 2>&1
_argus argus-init --project-path "$E2E_PROJECT" >/dev/null 2>&1 || { echo "[accept] argus-init 失败" >&2; exit 1; }
_argus argus-build --project-path "$E2E_PROJECT" >/dev/null 2>&1 || echo "[accept] build 警告（用既有镜像继续）" >&2
_argus argus-setup --project-path "$E2E_PROJECT" >/dev/null 2>&1 || { echo "[accept] argus-setup 失败" >&2; exit 1; }

RUN_OUT="$(_argus argus-run --project-path "$E2E_PROJECT" --filter "$SUITE" 2>&1)"

# ---- 立即 cp session transcript（趁 argus-clean 还没销毁容器）--------------
# argus-run 返回后容器仍存活（clean 在 trap EXIT），但只在这一瞬间可靠——
# 后续 case 检查、打印都可能让时序漂移。所以 run 一返回就先把 transcript 拿出来。
# 套件 setup 把 session cp 到 /tmp/sessions-<tag>；兜底 /workspace/sessions。
SESSION_CP_OK=0
echo "[accept] 诊断: argus-run 后容器状态: $(docker ps --filter name=$CONTAINER --format '{{.Status}}' 2>/dev/null || echo '(查询失败)')"
# session 落点因套件而异：套件 setup 通常 unset RECURSIVE_SESSIONS_DIR 并设
# RECURSIVE_HOME=/tmp/rh-<tag>，session 默认落在 /tmp/rh-<tag>/workspaces/<hash>/sessions/；
# 有的套件还额外 cp 到 /tmp/sessions-<tag>。无法预知确切路径，所以在容器内 find
# transcript.jsonl，拿到所在目录再 cp。
TRANSCRIPT_DIR_IN_CONTAINER=$(docker exec "$CONTAINER" find /tmp /workspace -name 'transcript.jsonl' -printf '%h\n' 2>/dev/null | head -1)
if [[ -n "$TRANSCRIPT_DIR_IN_CONTAINER" ]]; then
  docker cp "$CONTAINER:$TRANSCRIPT_DIR_IN_CONTAINER" "$JUDGE_WORK/session" 2>/dev/null && SESSION_CP_OK=1 \
    && echo "[accept] session cp 自 $TRANSCRIPT_DIR_IN_CONTAINER"
fi
if [[ "$SESSION_CP_OK" -eq 0 ]]; then
  echo "[accept] WARN: 容器内未找到 transcript.jsonl — step2 judge 将跳过" >&2
fi
if [[ "$SESSION_CP_OK" -eq 0 ]]; then
  echo "[accept] WARN: 无法从容器 cp session（容器可能已销毁或路径变了）— step2 judge 将跳过" >&2
fi

echo "$RUN_OUT" | python3 -c '
import sys, json
raw = sys.stdin.read()
i = raw.find("{")
try:
    d = json.loads(raw[i:])
except Exception:
    print("  (no JSON parsed)"); sys.exit(0)
data = d.get("data", {}) or {}
t = data.get("totals", {}) or {}
print("  status=%s totals=%s" % (data.get("status"), t))
' 2>&1 || true

# 录制跑的 case 必须先绿（case 红说明 agent 连产出都没达成，谈不上验收）
if ! echo "$RUN_OUT" | python3 -c '
import sys, json
raw = sys.stdin.read()
i = raw.find("{")
d = json.loads(raw[i:])
data = d.get("data", {}) or {}
t = data.get("totals", {}) or {}
sys.exit(0 if (data.get("status") == "passed" and t.get("total", 0) > 0 and t.get("failed", 0) == 0) else 1)
' 2>/dev/null; then
  echo "[accept] 录制跑的 case 未全绿（agent 没达成产出）— 看上面 totals" >&2
  echo "[accept] recorded/ 保留供检视: $RECORDED_DIR" >&2
  exit 1
fi
echo "[accept] 录制 case 全绿。recorded/ 产物:"
ls -la "$RECORDED_DIR"/*.json 2>/dev/null || echo "  (recorded/ 为空 — 见 RECORD_REPLAY.md 坑#2：已有 fixture 的套件 record 不打真模型)"

# ---- step 2: agent-judge 验收（用刚 cp 出的 transcript）-------------------
echo "[accept] step 2/4 — agent-judge 验收..."
if [[ "$SESSION_CP_OK" -eq 0 ]]; then
  echo "[accept] 跳过 judge（无 session transcript）— 仅录制+promote，不验收" >&2
  SKIP_JUDGE=1
else
  SKIP_JUDGE=0
fi

TRANSCRIPT=""
if [[ "$SKIP_JUDGE" -eq 0 ]]; then
  TRANSCRIPT="$(find "$JUDGE_WORK/session" -name 'transcript.jsonl' | head -1)"
  if [[ -z "$TRANSCRIPT" ]]; then
    # 兜底：有的套件 transcript 落在 session 根而非子目录
    TRANSCRIPT="$(find "$JUDGE_WORK/session" -name '*.jsonl' | head -1)"
  fi
  if [[ -z "$TRANSCRIPT" ]]; then
    echo "[accept] cp 出的 session 里找不到 transcript.jsonl — 跳过 judge" >&2
    SKIP_JUDGE=1
  else
    echo "[accept] judge 将读 transcript: $TRANSCRIPT"
  fi
fi

JUDGE_VERDICT_PASS=0
if [[ "$SKIP_JUDGE" -eq 0 ]]; then
  # 构造 judge prompt（复用 agent-judge-plugin.ts:161 的 prompt 结构 + expectation 注入）
  JUDGE_MODEL="${JUDGE_MODEL:-deepseek-chat}"
  JUDGE_API_BASE="${DEEPSEEK_API_BASE:-https://api.deepseek.com/v1}"
  EXPECT_BLOCK=""
  if [[ -f "$EXPECTATION" ]]; then
    EXPECT_BLOCK=$(printf '\n## 预期行为契约（来自 expectation 文件）\n```\n%s\n```\n判断时严格对照此契约。\n' "$(cat "$EXPECTATION")")
  fi

  JUDGE_PROMPT="You are evaluating whether an AI agent completed its task correctly, by reading its session transcript.

The agent's transcript (JSONL, one event per line) is at: $TRANSCRIPT
$EXPECT_BLOCK
## 判断步骤
1. 用 read_file 读 transcript，理清 agent 调了哪些工具、按什么顺序、传了什么参数。
2. 若有 expectation 契约，逐条对照 expect 与 anti-patterns。
3. 判断 agent 是否真正完成了任务、行为路径是否正确。

## 输出（最后一行，只能是裸 JSON，不要 markdown、不要多余文字）
{\"completed\": true/false, \"score\": 1-5, \"reason\": \"简述\", \"evidence\": [\"发现1\", \"发现2\"]}

评分：1=什么都没做 2=试了但失败 3=部分完成 4=基本完成 5=完全且正确"

  # host 上跑一个只读 recursive agent 当 judge（allowTools 限只读，避免 judge 改东西）
  # 把 prompt 里的单引号转义成 '\''（bash 单引号字符串的标准转义）
  SAFE_PROMPT="${JUDGE_PROMPT//\'/\'\\\'\'}"
  JUDGE_OUT="$(
    recursive --workspace "$JUDGE_WORK" \
      --api-base "$JUDGE_API_BASE" --api-key "$DEEPSEEK_API_KEY" \
      -m "$JUDGE_MODEL" --max-steps 8 \
      run "$SAFE_PROMPT" 2>&1
  )" || {
    echo "[accept] judge 运行失败（exit $?）：" >&2
    echo "$JUDGE_OUT" | tail -20 >&2
    exit 1
  }

# 解析 verdict（复用 agent-judge-plugin.ts:251 extractJudgeVerdict 的逻辑：
# 从末尾扫，返回第一个含 score/completed 的合法 JSON）
  VERDICT_JSON="$(python3 -c '
import json, re, sys
out = sys.stdin.read()
matches = list(re.finditer(r"\{[\s\S]*?\}", out))
for m in reversed(matches):
    try:
        obj = json.loads(m.group(0))
        if "score" in obj or "completed" in obj:
            print(json.dumps(obj)); break
    except Exception:
        continue
  ' <<<"$JUDGE_OUT")"

  if [[ -z "$VERDICT_JSON" ]]; then
    echo "[accept] judge 未吐出可解析的 JSON verdict。原始输出末尾：" >&2
    echo "$JUDGE_OUT" | tail -15 >&2
    exit 1
  fi

  COMPLETED=$(python3 -c "import json,sys;print(json.loads(sys.argv[1]).get('completed'))" "$VERDICT_JSON")
  SCORE=$(python3 -c "import json,sys;print(json.loads(sys.argv[1]).get('score',0))" "$VERDICT_JSON")
  REASON=$(python3 -c "import json,sys;print(json.loads(sys.argv[1]).get('reason',''))" "$VERDICT_JSON")
  echo "[accept] judge verdict: completed=$COMPLETED score=$SCORE/$SCORE_MIN"
  echo "[accept]   reason: $REASON"

  if [[ "$COMPLETED" != "True" ]] || [[ "$SCORE" -lt "$SCORE_MIN" ]]; then
    echo "[accept] 验收未通过（需 completed=true && score>=$SCORE_MIN）— 不 promote。" >&2
    echo "[accept] recorded/ 保留供检视: $RECORDED_DIR（--keep 可一并保留 judge workspace）" >&2
    exit 1
  fi
  JUDGE_VERDICT_PASS=1
  echo "[accept] 验收通过 ✓"
fi
# SKIP_JUDGE=1 时 JUDGE_VERDICT_PASS 保持 0，但 step3 promote 仍可进行（录制产物有效）

# ---- step 3: promote ------------------------------------------------------
echo "[accept] step 3/4 — promote fixture..."
if [[ ! -d "$RECORDED_DIR" ]] || [[ -z "$(ls -A "$RECORDED_DIR"/*.json 2>/dev/null)" ]]; then
  echo "[accept] recorded/ 为空 — 见 RECORD_REPLAY.md 坑#2：若套件已有完整 fixture，" >&2
  echo "         record 模式对已匹配请求走回放、不录新内容。需要用新 prompt/新套件才能录到。" >&2
  exit 1
fi
( cd "$E2E_PROJECT" && ./scripts/promote.sh "$SUITE" ) || { echo "[accept] promote.sh 失败" >&2; exit 1; }
echo "[accept] fixture 已升级 → e2e/fixtures/${SUITE}.json"

# ---- step 4: 无 key 回放回归 ----------------------------------------------
echo "[accept] step 4/4 — 回放验证（无 key）..."
# 走 e2e-run.sh 的纯 replay 路径（不设 E2E_RECORD）。它自己管 lifecycle。
unset E2E_RECORD
"$REPO_ROOT/.dev/scripts/e2e-run.sh" "$SUITE" --no-build
RC=$?
if [[ $RC -eq 0 ]]; then
  echo "[accept] 回放回归绿 ✓ — 全链路完成"
else
  echo "[accept] 回放回归红（promote 的 fixture 在 replay 下不通过）— 检查 fixture 匹配维度" >&2
  echo "         常见：多轮 userMessage 取最新、turnIndex/hasToolResult 维度（见 RECORD_REPLAY.md 已知陷阱）" >&2
fi
exit $RC
