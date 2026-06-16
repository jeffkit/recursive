#!/usr/bin/env bash
# e2e-gate.sh — argusai E2E smoke 门（供 flowcast self-improve flow 经
# .flowcast/gates.json 调用）。
#
# 职责（刻意单一）：在当前工作树里跑 recursive 的 argusai smoke 套件，
# 用退出码表达红/绿：
#   exit 0  → smoke 通过（绿灯）
#   exit !0 → smoke 失败 / 前置缺失（红灯）
#
# resume-fix（把失败喂回 agent 修一次）与 rollback 策略由 flow 的质量门
# 接管（见 .flowcast/gates.json 的 onFail），本脚本不做——它只回答
# 「现在这棵工作树的 smoke 过不过」。
#
# 这等价于老 self-improve.sh 里那段 argusai 编排的「纯判定」内核，
# 但抽成独立、可被任意门复用的命令。AGENTS.md 把 E2E smoke 列为强制门，
# 故前置缺失（mcp2cli / argusai-mcp / e2e.yaml）一律 HARD-FAIL（红灯），
# 不静默跳过。需要在含 Docker + argusai 的环境运行。

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

E2E_YAML="e2e/e2e.yaml"

# ---- 解析 mcp2cli ----------------------------------------------------------
MCP2CLI=""
for _c in "$HOME/.local/bin/mcp2cli" "/usr/local/bin/mcp2cli" "/opt/homebrew/bin/mcp2cli"; do
  [[ -x "$_c" ]] && { MCP2CLI="$_c"; break; }
done

# ---- 解析 argusai-mcp 入口（standalone → bundled → npx 兜底）---------------
ARGUSAI_MCP_BIN=""
for _root in "$(npm root -g 2>/dev/null)" \
    "$HOME/.local/share/fnm/node-versions"/*/installation/lib/node_modules; do
  if [[ -f "$_root/argusai-mcp/dist/index.js" ]]; then
    ARGUSAI_MCP_BIN="$_root/argusai-mcp/dist/index.js"; break
  fi
  if [[ -f "$_root/argusai-cli/node_modules/argusai-mcp/dist/index.js" ]]; then
    ARGUSAI_MCP_BIN="$_root/argusai-cli/node_modules/argusai-mcp/dist/index.js"; break
  fi
done
_MCP_STDIO_CMD=""
if [[ -n "$ARGUSAI_MCP_BIN" ]]; then
  _MCP_STDIO_CMD="node $ARGUSAI_MCP_BIN"
elif command -v npx >/dev/null 2>&1; then
  _MCP_STDIO_CMD="npx argusai-mcp"
fi

# ---- HARD-GATE：前置缺失即红灯 ---------------------------------------------
_missing=()
[[ -n "$MCP2CLI" ]]        || _missing+=("mcp2cli（uv tool install mcp2cli）")
[[ -n "$_MCP_STDIO_CMD" ]] || _missing+=("argusai-mcp（npm i -g argusai-mcp）")
[[ -f "$E2E_YAML" ]]       || _missing+=("$E2E_YAML")
if [[ ${#_missing[@]} -gt 0 ]]; then
  echo "[e2e-gate] HARD-FAIL — 缺少 E2E 前置：" >&2
  printf '  - %s\n' "${_missing[@]}" >&2
  exit 3
fi

# ---- 构建 e2e 插件（首次或 src 变动后）-------------------------------------
if [[ -f "e2e/plugins/package.json" ]] && [[ ! -f "e2e/plugins/dist/index.js" || \
    "e2e/plugins/src" -nt "e2e/plugins/dist/index.js" ]]; then
  # `npm run build` 走 tsc，需要 @types/node；node_modules 在 clean worktree
  # 下被 .gitignore 排除。 缺失时 `tsc` 报 TS2688，整个门会变红。 在 build
  # 之前确认 node_modules 存在且与 lockfile / package.json 同步。
  if [[ ! -d "e2e/plugins/node_modules" || \
      "e2e/plugins/package.json" -nt "e2e/plugins/node_modules" || \
      ( -f "e2e/plugins/pnpm-lock.yaml" && \
        "e2e/plugins/pnpm-lock.yaml" -nt "e2e/plugins/node_modules" ) ]]; then
    echo "[e2e-gate] 安装 e2e/plugins 依赖（node_modules 缺失或过期）..."
    (cd e2e/plugins && npm install --no-audit --no-fund 2>&1) \
      || { echo "[e2e-gate] e2e/plugins npm install 失败" >&2; exit 4; }
  fi
  echo "[e2e-gate] 构建 e2e/plugins ..."
  (cd e2e/plugins && npm run build 2>&1) || { echo "[e2e-gate] e2e/plugins build 失败" >&2; exit 4; }
fi

# ---- 重建二进制，让容器拿到新代码 ------------------------------------------
cargo build -q 2>/dev/null || { echo "[e2e-gate] cargo build 失败" >&2; exit 4; }

# ---- 跑 smoke --------------------------------------------------------------
WORKTREE_ID="wt-$(git rev-parse --short HEAD 2>/dev/null || echo main)"
export WORKTREE_ID
E2E_PROJECT="$(pwd)/e2e"
SESSION="argusai-$WORKTREE_ID"

# _argus：调用 argusai MCP 工具并检测 JSON `"success":false`（mcp2cli 对工具级错误返回 exit 0）
_argus() {
  local s="$1"; shift
  local out
  out="$("$MCP2CLI" --session "$s" "$@" 2>&1)"
  local rc=$?
  echo "$out"
  # 若 JSON 含 "success":false，视为失败
  if echo "$out" | python3 -c "import sys,json; d=json.load(sys.stdin); sys.exit(0 if d.get('success',True) else 1)" 2>/dev/null; then
    return $rc
  else
    return 1
  fi
}

# 有状态 MCP server：init/setup/run 共享同一进程
WORKTREE_ID="$WORKTREE_ID" "$MCP2CLI" --mcp-stdio "$_MCP_STDIO_CMD" \
  --session-start "$SESSION" >/dev/null 2>&1

INIT_LOG="$(pwd)/.flowcast/runs/e2e-init-${SESSION}.log"
mkdir -p "$(dirname "$INIT_LOG")"
if ! _argus "$SESSION" argus-init --project-path "$E2E_PROJECT" >"$INIT_LOG" 2>&1; then
  echo "[e2e-gate] argus-init 失败 — 见 $INIT_LOG" >&2
  head -20 "$INIT_LOG" >&2
  "$MCP2CLI" --session-stop "$SESSION" >/dev/null 2>&1 || true
  exit 5
fi
_argus "$SESSION" argus-setup --project-path "$E2E_PROJECT" 2>&1 | tail -3

RC=1
if _argus "$SESSION" argus-run --project-path "$E2E_PROJECT" --filter "smoke" 2>&1 | grep -q '"passed"'; then
  echo "[e2e-gate] smoke PASSED ✓"
  RC=0
else
  echo "[e2e-gate] smoke FAILED ✗" >&2
  _argus "$SESSION" argus-run --project-path "$E2E_PROJECT" --filter "smoke" 2>&1 | tail -30 >&2
fi

_argus "$SESSION" argus-clean --project-path "$E2E_PROJECT" >/dev/null 2>&1 || true
"$MCP2CLI" --session-stop "$SESSION" >/dev/null 2>&1 || true
exit "$RC"
