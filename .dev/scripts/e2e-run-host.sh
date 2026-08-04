#!/usr/bin/env bash
# e2e-run-host.sh — 在 host 模式下通过 argusai-mcp 跑 e2e suite。
#
# 用 argusai 的 HostRuntime（不构建 Docker 镜像、不启服务容器），直接在
# host 上跑测试命令。aimock 通过端口映射运行在 localhost。
#
# 这是验证 HostRuntime 端到端能力的脚本。目前只跑 smoke suite——其余
# suite 的 /workspace 路径需要改成 host 路径后才能跑（后续工作）。
#
# 用法：
#   e2e-run-host.sh                # 跑 smoke
#   e2e-run-host.sh session        # 跑指定 suite
#
# 前置：npm link argusai-mcp（指向本地 argusai 仓的 host-runtime 分支）
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

SUITE="${1:-smoke}"

# ── 前置检查 ──
command -v mcp2cli >/dev/null 2>&1 || { echo "error: mcp2cli required"; exit 3; }
command -v jq >/dev/null 2>&1 || { echo "error: jq required"; exit 3; }
RECURSIVE_BIN="$REPO_ROOT/target/release/recursive"
[[ -x "$RECURSIVE_BIN" ]] || { echo "error: cargo build --release first"; exit 3; }

# 解析 argusai-mcp 入口（应该已被 npm link 指向本地）
ARGUSAI_MCP_BIN=""
for _root in "$(npm root -g 2>/dev/null)" \
    "$HOME/.local/share/fnm/node-versions"/*/installation/lib/node_modules; do
  if [[ -f "$_root/argusai-mcp/dist/index.js" ]]; then
    ARGUSAI_MCP_BIN="$_root/argusai-mcp/dist/index.js"; break
  fi
done
[[ -n "$ARGUSAI_MCP_BIN" ]] || { echo "error: argusai-mcp not found (npm link?)"; exit 3; }
echo "[e2e-host] using argusai-mcp: $ARGUSAI_MCP_BIN"

# ── host 模式环境变量 ──
export E2E_HOST_MODE=1
export WORKTREE_ID="host-$$"
export E2E_AIMOCK_PORT="${E2E_AIMOCK_PORT:-4010}"
# recursive provider env — in Docker mode these are set by e2e.yaml's
# container.environment; in host mode we set them globally so the recursive
# binary (invoked by YAML exec steps) inherits them via HostRuntime.
export RECURSIVE_PROVIDER_TYPE=openai
export RECURSIVE_API_KEY=mock-key
export RECURSIVE_API_BASE=http://aimock:4010/v1
export RECURSIVE_RATE_LIMIT_BURST=200
export RECURSIVE_RATE_LIMIT_RPM=6000
# E2E_WORKSPACE_DIR: HostRuntime transparently maps /workspace → this dir
# in exec commands, so YAML suites with container paths work on host.
WORKSPACE_DIR=$(mktemp -d /tmp/e2e-host-ws-XXXXXX)
export E2E_WORKSPACE_DIR="$WORKSPACE_DIR"
# recursive binary 在 PATH 上（用主仓的 release 产物）
export PATH="$REPO_ROOT/target/release:$PATH"

# ── aimock URL 适配 ──
# HostRuntime (argusai 0.15.2+) automatically maps aimock:PORT → localhost:PORT
# in exec commands, so YAML suites with http://aimock:4010 work on host.
# No wrapper needed.

# ── 容器绝对路径 → host 路径映射（SDK suites 引用 /sdk/...）──
# E2E_HOST_REPLACEMENTS is HostRuntime's generic old=new substitution. SDK
# suites require('/sdk/typescript/...') / install from /sdk/python — container
# paths that don't exist on the host. Map them back to the repo checkout so
# the SDK e2e suites (20/21) resolve without per-suite edits.
export E2E_HOST_REPLACEMENTS="/sdk/typescript=$REPO_ROOT/sdk/typescript /sdk/python=$REPO_ROOT/sdk/python"

# ── host 模式的 e2e 配置（动态生成，避免改原 e2e.yaml）──
# 关键差异：runtime: host、无 service/build、aimock 走 localhost。
# Suite 注册表从 e2e/e2e.yaml 抽取（全部 41 suite），改成绝对路径，
# 这样 --filter <id> 能命中任意 suite，与 Docker 模式对齐。
HOST_E2E_YAML="$WORKSPACE_DIR/e2e-host.yaml"
MASTER_YAML="$REPO_ROOT/e2e/e2e.yaml"
SUITES_BLOCK="$(python3 - "$MASTER_YAML" "$REPO_ROOT" <<'PY'
import sys, yaml
master, repo = sys.argv[1], sys.argv[2]
doc = yaml.safe_load(open(master))
lines = []
for s in doc.get("tests", {}).get("suites", []):
    name = s["name"].replace('"', '\\"')
    sid = s["id"]
    file = f"{repo}/e2e/tests/{s['file'].split('/')[-1]}" \
        if "/" not in s["file"] else f"{repo}/e2e/{s['file']}"
    lines.append(f'    - name: "{name}"\n      id: {sid}\n      file: {file}')
print("\n".join(lines))
PY
)"

cat > "$HOST_E2E_YAML" <<EOF
version: "1"

project:
  name: recursive-agent-host
  description: "Host-mode E2E (no Docker containers)"

runtime:
  type: host

isolation:
  namespace: "{{env.WORKTREE_ID}}"

plugins:
  - $REPO_ROOT/e2e/plugins/dist/index.js

# No service section — host mode runs the binary directly.
# aimock is started by the plugin on localhost:E2E_AIMOCK_PORT.

tests:
  suites:
$SUITES_BLOCK
EOF

# 生成 host 版本的 e2e 项目目录（让 argusai-init 找到 e2e-host.yaml）
HOST_E2E_PROJECT="$WORKSPACE_DIR/e2e-project"
mkdir -p "$HOST_E2E_PROJECT"
cp "$HOST_E2E_YAML" "$HOST_E2E_PROJECT/e2e.yaml"

# ── 清理 trap ──
AIMOCK_NAME="${WORKTREE_ID}-aimock"
cleanup() {
  local rc=$?
  docker rm -f "$AIMOCK_NAME" >/dev/null 2>&1 || true
  # Drop the WORKTREE_ID-scoped network this run created. Leftover
  # argusai-* networks accumulate and exhaust Docker's subnet pool
  # ("all predefined address pools have been fully subnetted"), which
  # breaks later Docker-mode setups. Only remove this run's network.
  docker network rm "argusai-${WORKTREE_ID}-network" >/dev/null 2>&1 || true
  rm -rf "$WORKSPACE_DIR"
  exit "$rc"
}
trap cleanup EXIT

echo "[e2e-host] workspace: $WORKSPACE_DIR (/workspace → $WORKSPACE_DIR)"

echo "[e2e-host] running suite: $SUITE"

# ── 通过 mcp2cli 调用 argusai-mcp ──
SESSION="argusai-host-$$"

_argus() {
  local s="$1"; shift
  local out
  out="$("$MCP2CLI" --session "$s" "$@" 2>&1)"
  local rc=$?
  echo "$out"
  if echo "$out" | python3 -c "import sys,json; d=json.load(sys.stdin); sys.exit(0 if d.get('success',True) else 1)" 2>/dev/null; then
    return $rc
  else
    return 1
  fi
}

MCP2CLI=$(command -v mcp2cli)

# 启动 MCP server + init
WORKTREE_ID="$WORKTREE_ID" E2E_HOST_MODE=1 E2E_WORKSPACE_DIR="$E2E_WORKSPACE_DIR" "$MCP2CLI" --mcp-stdio "node $ARGUSAI_MCP_BIN" \
  --session-start "$SESSION" >/dev/null 2>&1

echo "[e2e-host] argus-init…"
INIT_LOG="$WORKSPACE_DIR/init.log"
if ! _argus "$SESSION" argus-init --project-path "$HOST_E2E_PROJECT" >"$INIT_LOG" 2>&1; then
  echo "[e2e-host] argus-init FAILED — see $INIT_LOG" >&2
  head -20 "$INIT_LOG" >&2
  "$MCP2CLI" --session-stop "$SESSION" >/dev/null 2>&1 || true
  exit 5
fi

echo "[e2e-host] argus-setup…"
_argus "$SESSION" argus-setup --project-path "$HOST_E2E_PROJECT" 2>&1 | tail -5

echo "[e2e-host] argus-run --filter ${SUITE}..."
RUN_LOG="$WORKSPACE_DIR/run.log"
_argus "$SESSION" argus-run --project-path "$HOST_E2E_PROJECT" --filter "$SUITE" >"$RUN_LOG" 2>&1

# 判定结果
RC=1
if python3 -c '
import sys, json
raw = open(sys.argv[1]).read()
i = raw.find("{")
if i < 0: sys.exit(1)
d = json.loads(raw[i:])
# mcp2cli wraps the tool result as {"success": ..., "data": <tool-payload>}.
# The actual run totals live under data.totals (or data itself if flat).
if isinstance(d.get("data"), dict):
    d = d["data"]
totals = d.get("totals", d)
total = totals.get("total", 0)
failed = totals.get("failed", 0)
status = totals.get("status", d.get("status", ""))
if total > 0 and failed == 0 and status in ("passed", ""):
    print(f"PASS: {total} cases, 0 failed")
    sys.exit(0)
print(f"FAIL: {json.dumps(totals)}")
sys.exit(1)
' "$RUN_LOG" 2>/dev/null; then
  RC=0
fi

echo ""
echo "[e2e-host] run output:"
cat "$RUN_LOG" | tail -20

"$MCP2CLI" --session-stop "$SESSION" >/dev/null 2>&1 || true
exit $RC
