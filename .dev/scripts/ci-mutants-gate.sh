#!/usr/bin/env bash
# ci-mutants-gate.sh — 把 cargo-mutants 门禁下沉到 GitHub Actions（self-improve
# 沙箱只做提交快照 + 轮询，不再本机跑 40-150 分钟的变异测试）。
#
# 用法（.flowcast/gates.json 的 cmd）：
#   bash .dev/scripts/ci-mutants-gate.sh <agent|cli|tui>
# 环境变量：
#   RECURSIVE_MUTANTS_ENGINE  auto(默认) | ci | local
#       auto = gh 可用且 dispatch 成功用 CI，否则自动回退本机脚本（基础设施
#              问题绝不误回滚；只有 CI 真红才算门红）
#   CI_MUTANTS_MAX_WAIT      轮询死线秒数（默认 3300；gates.json 里按 crate 调）
#   CI_GATE_KEEP_BRANCH=1    保留远端快照分支（默认跑完即删）
#
# 工作原理（关键：零 git 状态污染）：
#   1. 用临时 GIT_INDEX_FILE 把工作树状态写成 tree/commit 对象（write-tree +
#      commit-tree），**不碰真实 index/HEAD/工作树** —— 评审步的 `git diff HEAD`
#      和落地步的 commit 语义完全不受影响（引擎门禁跑在 commit 之前）。
#   2. 把快照 commit 直接 push 成 refs/heads/ci-gate/<sha8>（裸 sha push）。
#   3. `gh workflow run mutants.yml --ref <branch> -f scope=incremental` ——
#      增量作用域由 workflow 里的脚本自算（main...HEAD = 本次目标改动）。
#   4. 轮询到出结论；红 → 拉取 MISSED/survivor 段喂给 resume-fix（引擎会把
#      本脚本全部输出写进 .gate-<name>-output.log 供 fixer 读）。
#
# 作用域自跳过与本地脚本同语义：diff 未触及对应 crate 的 src/ 时 exit 0，
# 不烧 CI 时间。cargo-mutants 缺失/gh 缺失/超时/取消的判据见各分支注释。
set -uo pipefail

CRATE="${1:?usage: ci-mutants-gate.sh <agent|cli|tui>}"
ENGINE="${RECURSIVE_MUTANTS_ENGINE:-auto}"
MAX_WAIT="${CI_MUTANTS_MAX_WAIT:-3300}"
REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$REPO_ROOT" || exit 2

case "$CRATE" in
  agent) LOCAL_SCRIPT=".dev/scripts/agent-mutants.sh" ;;
  cli)   LOCAL_SCRIPT=".dev/scripts/cli-mutants.sh" ;;
  tui)   LOCAL_SCRIPT=".dev/scripts/tui-mutants.sh" ;;
  *) echo "[ci-mutants] unknown crate '$CRATE' (agent|cli|tui)" >&2; exit 2 ;;
esac

run_local() {
  echo "[ci-mutants/$CRATE] falling back to local engine: $LOCAL_SCRIPT" >&2
  bash "$LOCAL_SCRIPT"
}

# ── 前置：gh 可用才走 CI ─────────────────────────────────────────────
if ! command -v gh >/dev/null 2>&1; then
  echo "[ci-mutants/$CRATE] gh not on PATH" >&2
  [[ "$ENGINE" == "ci" ]] && exit 4
  run_local
  exit $?
fi
if ! gh auth status >/dev/null 2>&1; then
  echo "[ci-mutants/$CRATE] gh not authenticated" >&2
  [[ "$ENGINE" == "ci" ]] && exit 4
  run_local
  exit $?
fi

# ── 1. 快照：临时 index 写 tree/commit，不碰真实 git 状态 ────────────
TMP_INDEX="$(mktemp /tmp/ci-gate-index-XXXXXX)"
cleanup() {
  local rc=$?
  rm -f "$TMP_INDEX"
  # 快照分支默认删掉（下次 attempt 会重新 push 新快照；留着只会堆积）。
  if [[ "${CI_GATE_KEEP_BRANCH:-0}" != "1" && -n "${BRANCH:-}" ]] && [[ "$rc" -ne 3 ]]; then
    git push origin --delete "refs/heads/$BRANCH" >/dev/null 2>&1 || true
  fi
  exit "$rc"
}
trap cleanup EXIT

export GIT_INDEX_FILE="$TMP_INDEX"
git add -A 2>/dev/null || true
TREE="$(git write-tree 2>/dev/null)" || { echo "[ci-mutants/$CRATE] write-tree failed" >&2; [[ "$ENGINE" == "ci" ]] && exit 4; run_local; exit $?; }
HEAD_SHA="$(git rev-parse HEAD)"
SHA="$(echo "ci-gate snapshot (crate=$CRATE)" | git commit-tree "$TREE" -p "$HEAD_SHA")"
unset GIT_INDEX_FILE
BRANCH="ci-gate-$(echo "$SHA" | cut -c1-8)"

# ── 2. 作用域自跳过（与本地脚本同语义，不烧 CI）─────────────────────
BASE="$(git rev-parse -q --verify main || git rev-parse -q --verify origin/main || true)"
if [[ -z "$BASE" ]]; then
  echo "[ci-mutants/$CRATE] no base ref (main/origin/main) — cannot scope" >&2
  [[ "$ENGINE" == "ci" ]] && exit 4
  run_local
  exit $?
fi
case "$CRATE" in
  agent) PATHSPEC=('src/*.rs' ':(exclude)src/weixin/*' ':(exclude)src/test_util.rs') ;;
  cli)   PATHSPEC=('crates/recursive-cli/src') ;;
  tui)   PATHSPEC=('crates/recursive-tui/src/*.rs') ;;
esac
CHANGED="$(git diff --name-only "$BASE...$SHA" -- "${PATHSPEC[@]}" 2>/dev/null || true)"
if [[ -z "$CHANGED" ]]; then
  echo "[ci-mutants/$CRATE] skip: no $CRATE source changed vs main (local scripts would self-skip too)"
  exit 0
fi

# ── 3. push 快照分支 + dispatch ─────────────────────────────────────
if ! git push origin "$SHA:refs/heads/$BRANCH" >/dev/null 2>&1; then
  echo "[ci-mutants/$CRATE] push failed" >&2
  [[ "$ENGINE" == "ci" ]] && exit 4
  run_local
  exit $?
fi
if ! gh workflow run mutants.yml --ref "$BRANCH" -f scope=incremental 2>&1; then
  echo "[ci-mutants/$CRATE] workflow dispatch failed (stale worktree without mutants.yml?)" >&2
  [[ "$ENGINE" == "ci" ]] && exit 4
  run_local
  exit $?
fi
echo "[ci-mutants/$CRATE] snapshot $SHA pushed as $BRANCH; mutants.yml dispatched (scope=incremental)"

# ── 4. 轮询到出结论 ─────────────────────────────────────────────────
RUN_ID=""
DEADLINE=$(( SECONDS + MAX_WAIT ))
ERR_STREAK=0
while (( SECONDS < DEADLINE )); do
  sleep 20
  OUT="$(gh run list -R jeffkit/recursive --workflow=mutants.yml --branch "$BRANCH" \
          --event workflow_dispatch --limit 1 \
          --json databaseId,status,conclusion 2>/dev/null || true)"
  if [[ -z "$OUT" || "$OUT" == "[]" ]]; then
    # dispatch 异步落账有延迟，前几次查不到属正常；连续失败 = 轮询基础设施
    # 问题 → 回退本机引擎（同 dispatch 失败的处理哲学：infra 问题不误回滚）
    (( ERR_STREAK++ ))
    if (( ERR_STREAK > 15 )); then
      echo "[ci-mutants/$CRATE] polling failed 15x (gh/API flake) — falling back to local" >&2
      [[ "$ENGINE" == "ci" ]] && exit 4
      run_local
      exit $?
    fi
    continue
  fi
  STATUS="$(echo "$OUT" | python3 -c 'import sys,json; d=json.load(sys.stdin); print(d[0]["status"] if d else "")' 2>/dev/null || true)"
  CONCL="$(echo "$OUT" | python3 -c 'import sys,json; d=json.load(sys.stdin); print(d[0].get("conclusion") or "")' 2>/dev/null || true)"
  RUN_ID="$(echo "$OUT" | python3 -c 'import sys,json; d=json.load(sys.stdin); print(d[0]["databaseId"] if d else "")' 2>/dev/null || true)"
  if [[ "$STATUS" == "completed" ]]; then
    break
  fi
  echo "[ci-mutants/$CRATE] run $RUN_ID status=$STATUS (elapsed ${SECONDS}s/$MAX_WAIT)"
done

if [[ "$STATUS" != "completed" ]]; then
  echo "[ci-mutants/$CRATE] TIMEOUT after ${MAX_WAIT}s (run ${RUN_ID:-unknown}) — cancelling remote run" >&2
  [[ -n "$RUN_ID" ]] && gh run cancel "$RUN_ID" -R jeffkit/recursive >/dev/null 2>&1 || true
  # exit 3：看门狗超时（gates.json 失败模式 #8 的教训：绝不能拿截断报告当幸存者清单）
  exit 3
fi

RUN_URL="https://github.com/jeffkit/recursive/actions/runs/$RUN_ID"
if [[ "$CONCL" == "success" ]]; then
  echo "[ci-mutants/$CRATE] CI PASSED ✓ ($RUN_URL)"
  exit 0
fi

# ── 5. 红：把幸存者段喂给 resume-fix ────────────────────────────────
echo "[ci-mutants/$CRATE] CI FAILED ($CONCL): $RUN_URL" >&2
echo "" >&2
echo "The CI re-runs automatically after your edits — do NOT run this gate command" >&2
echo "yourself (it pushes a snapshot and waits on GitHub). Strengthen the tests that" >&2
echo "fail to kill the surviving mutants listed below." >&2
echo "" >&2
echo "--- survivor report (from CI log) ---"
gh run view "$RUN_ID" -R jeffkit/recursive --log 2>/dev/null \
  | grep -E "MISSED|missed|survived|timeout" \
  | grep -v "0 missed" | head -60 || true
exit 2
