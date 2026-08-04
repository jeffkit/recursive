#!/usr/bin/env bash
# e2e-local.sh — 本地 smoke 测试，旁路 argusai/Docker 镜像构建。
#
# 复刻 e2e/tests/00-smoke.yaml 的两个场景（write_file + read_file），直接在
# host 上跑 recursive binary，不经过 argusai/mcp2cli/镜像构建。
# 仍需 Docker —— 但只用于跑 aimock mock 服务（一个简单的端口映射容器），
# 不走 argusai 的网络编排和镜像构建链路。
#
# 耗时 ~10s（binary 已有，只跑两个命令 + 断言），对比 Docker 全量 e2e 的
# 10-40min。供 e2e-gate.sh 的本地快路径调用。
#
# 用法：
#   e2e-local.sh                     # 跑 smoke（默认端口 4010）
#   E2E_AIMOCK_PORT=4011 e2e-local.sh  # 指定 aimock 端口（并发安全）
#
# 退出码：0=全过，1=有失败，3=前置缺失（docker/jq/binary）。
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

SECONDS=0

# ── 前置检查 ──────────────────────────────────────────────────────────
if ! command -v docker >/dev/null 2>&1; then
  echo "error: docker required (for aimock mock service)" >&2
  exit 3
fi
if ! command -v jq >/dev/null 2>&1; then
  echo "error: jq required (for session assertions)" >&2
  exit 3
fi
RECURSIVE_BIN="$REPO_ROOT/target/release/recursive"
if [[ ! -x "$RECURSIVE_BIN" ]]; then
  echo "error: $RECURSIVE_BIN not found — run 'cargo build --release -p recursive-cli' first" >&2
  exit 3
fi

# ── 清理 trap（aimock 容器 + 临时目录）──────────────────────────────
AIMOCK_NAME=""
TMPDIR_BASE=""
cleanup() {
  local rc=$?
  if [[ -n "$AIMOCK_NAME" ]]; then
    docker rm -f "$AIMOCK_NAME" >/dev/null 2>&1 || true
  fi
  if [[ -n "$TMPDIR_BASE" && -d "$TMPDIR_BASE" ]]; then
    rm -rf "$TMPDIR_BASE"
  fi
  exit "$rc"
}
trap cleanup EXIT

# ── 起 aimock（本地端口映射，不走 argusai 网络）──────────────────────
AIMOCK_PORT="${E2E_AIMOCK_PORT:-4010}"
AIMOCK_NAME="e2e-local-aimock-$$"
FIXTURES_DIR="$REPO_ROOT/e2e/fixtures"

if ! docker run -d --rm --name "$AIMOCK_NAME" \
  -p "${AIMOCK_PORT}:4010" \
  -v "${FIXTURES_DIR}:/fixtures:ro" \
  ghcr.io/copilotkit/aimock -f /fixtures -h 0.0.0.0 >/dev/null 2>&1; then
  echo "error: failed to start aimock container on port $AIMOCK_PORT" >&2
  echo "  (is the port already in use? set E2E_AIMOCK_PORT to change)" >&2
  exit 3
fi

# 等 aimock ready（最多 5 秒）
for _ in 1 2 3 4 5; do
  if curl -sf "http://localhost:${AIMOCK_PORT}/v1/models" >/dev/null 2>&1; then
    break
  fi
  sleep 1
done

# ── 准备临时工作区 ───────────────────────────────────────────────────
TMPDIR_BASE=$(mktemp -d /tmp/e2e-local-XXXXXX)

API_BASE="http://localhost:${AIMOCK_PORT}/v1"

# ── 场景 1: write_file ────────────────────────────────────────────────
WS1="$TMPDIR_BASE/smoke-01"
RH1="$TMPDIR_BASE/rh-01"
mkdir -p "$WS1"

# RECURSIVE_HOME 隔离 session/shadow-git；清空 RECURSIVE_SESSIONS_DIR 让
# session 落在 RECURSIVE_HOME 下（和 00-smoke.yaml 的 unset 逻辑一致）。
# 选项放在 run 前面（recursive 的全局 options 不是 run 子命令的）。
env -u RECURSIVE_SESSIONS_DIR RECURSIVE_HOME="$RH1" \
  "$RECURSIVE_BIN" --workspace "$WS1" \
    --api-base "$API_BASE" --api-key mock-key \
    --provider openai -m mock-chat --max-steps 5 \
    run "Create a file called smoke.txt with content 'ok'" \
    >/dev/null 2>&1
RC1=$?

# ── 场景 2: read_file ─────────────────────────────────────────────────
WS2="$TMPDIR_BASE/smoke-02"
RH2="$TMPDIR_BASE/rh-02"
mkdir -p "$WS2"
echo 'test content' > "$WS2/input.txt"

env -u RECURSIVE_SESSIONS_DIR RECURSIVE_HOME="$RH2" \
  "$RECURSIVE_BIN" --workspace "$WS2" \
    --api-base "$API_BASE" --api-key mock-key \
    --provider openai -m mock-chat --max-steps 5 \
    run "Read the file input.txt and tell me what it contains" \
    >/dev/null 2>&1
RC2=$?

# ── 断言 ──────────────────────────────────────────────────────────────
FAIL=0

# session 断言函数（复刻 session-plugin.ts 的逻辑，用 jq）
# 用法: assert_session <session_dir> <scenario_name>
# 校验: .meta.json 存在、status completed/success、transcript.jsonl 有 user+assistant role
assert_session() {
  local sess_dir="$1" name="$2"
  local meta="$sess_dir/.meta.json" jsonl="$sess_dir/transcript.jsonl"

  if [[ ! -f "$meta" || ! -f "$jsonl" ]]; then
    echo "  FAIL: $name — session files missing ($meta / $jsonl)" >&2
    return 1
  fi

  # status: 容忍 completed（现代）和 success（legacy 别名）
  local st
  st=$(jq -r '.status // empty' "$meta" 2>/dev/null)
  if [[ "$st" != "completed" && "$st" != "success" ]]; then
    echo "  FAIL: $name — status='$st' (expected completed/success)" >&2
    return 1
  fi

  # hasRoles: user + assistant 必须都在
  local roles
  roles=$(jq -r '.role // empty' "$jsonl" 2>/dev/null | sort -u)
  for want in user assistant; do
    if ! echo "$roles" | grep -qx "$want"; then
      echo "  FAIL: $name — missing role '$want' (found: $(echo "$roles" | tr '\n' ' '))" >&2
      return 1
    fi
  done

  return 0
}

# 用法: assert_tool_called <jsonl_path> <tool_name> <scenario_name>
assert_tool_called() {
  local jsonl="$1" tool="$2" name="$3"
  if ! jq -r '.tool_calls[]?.name // empty' "$jsonl" 2>/dev/null | grep -qx "$tool"; then
    echo "  FAIL: $name — tool '$tool' not called" >&2
    return 1
  fi
  return 0
}

echo "[e2e-local] asserting results…"

# ── 场景 1 断言 ──
echo "  scenario 1 (write_file):"
if [[ $RC1 -ne 0 ]]; then
  echo "  FAIL: smoke-01 — recursive exited $RC1" >&2
  FAIL=1
else
  # 文件存在 + 内容含 'ok'
  if [[ ! -f "$WS1/smoke.txt" ]]; then
    echo "  FAIL: smoke-01 — smoke.txt not created" >&2
    FAIL=1
  elif ! grep -q 'ok' "$WS1/smoke.txt" 2>/dev/null; then
    echo "  FAIL: smoke-01 — smoke.txt exists but missing 'ok'" >&2
    FAIL=1
  else
    echo "  ok: smoke-01 — smoke.txt created with 'ok'"
  fi
  # session 校验（BSD/GNU find 兼容：不用 -printf，改用 dirname）
  SESS1=$(find "$RH1" -name '.meta.json' 2>/dev/null | head -1)
  SESS1="${SESS1%/.meta.json}"
  if [[ -z "$SESS1" ]]; then
    echo "  FAIL: smoke-01 — no session found under $RH1" >&2
    FAIL=1
  else
    assert_session "$SESS1" "smoke-01" || FAIL=1
    assert_tool_called "$SESS1/transcript.jsonl" "write_file" "smoke-01" || FAIL=1
    [[ $FAIL -eq 0 ]] && echo "  ok: smoke-01 — session valid (write_file recorded)"
  fi
fi

# ── 场景 2 断言 ──
echo "  scenario 2 (read_file):"
if [[ $RC2 -ne 0 ]]; then
  echo "  FAIL: smoke-02 — recursive exited $RC2" >&2
  FAIL=1
else
  SESS2=$(find "$RH2" -name '.meta.json' 2>/dev/null | head -1)
  SESS2="${SESS2%/.meta.json}"
  if [[ -z "$SESS2" ]]; then
    echo "  FAIL: smoke-02 — no session found under $RH2" >&2
    FAIL=1
  else
    assert_session "$SESS2" "smoke-02" || FAIL=1
    assert_tool_called "$SESS2/transcript.jsonl" "read_file" "smoke-02" || FAIL=1
    [[ $FAIL -eq 0 ]] && echo "  ok: smoke-02 — session valid (read_file recorded)"
  fi
fi

# ── 结果 ──
echo ""
if [[ "$FAIL" -eq 0 ]]; then
  echo "[e2e-local] ✅ smoke PASS (2 scenarios, ${SECONDS}s)"
  exit 0
else
  echo "[e2e-local] ❌ smoke FAIL (${SECONDS}s)"
  exit 1
fi
